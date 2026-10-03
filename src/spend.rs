//! LUD-25 spends: which note a `k1` names, and whether it opens it here.
//!
//! `lnurlcash-core` decodes the spend and applies the mint's own rules: the
//! leaf version and `OP_SUCCESSx` refusal, and time claims against the mint's
//! clock. Whether the witness opens `Q` is Bitcoin Core's call, through
//! `lnurlcash-kernel`, for every key path and every leaf, as lnurl-mint does.

use lnurlcash_core::spend::{Spend, check_leaf, check_time_claim, decode_spend};

pub use lnurlcash_core::spend::decode_note;

/// What a key-path failure, an unknown note and a spent note all look like
/// from outside: explaining a failed signature only helps someone guess.
pub const INVALID_K1: &str = crate::db::INVALID_K1;

/// A decoded `k1`: the note it names.
#[derive(Debug, Clone)]
pub struct ParsedK1 {
    pub note_id: String,
    spend: Spend,
}

/// hex(Q) of whatever goes where a `cp1` goes: a `cp1` or a bearer `h`.
pub fn note_id_of_ref(value: &str) -> Option<String> {
    decode_note(value).map(hex::encode)
}

/// The note `k1` claims to spend, or `None` if it is no current-format spend.
/// A 65-byte legacy `ck1` is refused: it binds to no domain.
pub fn parse(k1: &str) -> Option<ParsedK1> {
    let spend = decode_spend(k1)?;
    if matches!(spend, Spend::LegacyKeyPath { .. }) {
        return None;
    }
    Some(ParsedK1 {
        note_id: hex::encode(spend.output_key()),
        spend,
    })
}

/// Core's verdict on one domain. The library failing to run is logged and
/// counts as no, never as yes.
fn core_says(result: Result<bool, lnurlcash_kernel::KernelError>) -> bool {
    result.unwrap_or_else(|e| {
        log::warn!("libbitcoinkernel: {e}");
        false
    })
}

/// `None` if `parsed` opens its note at any of `domains` at time `now`, else
/// why not. `locked_at` is when the mint credited the note.
pub fn verify(parsed: &ParsedK1, locked_at: u64, domains: &[String], now: u64) -> Option<String> {
    match &parsed.spend {
        Spend::KeyPath {
            output_key,
            signature,
        } => {
            let opens = domains.iter().any(|domain| {
                core_says(lnurlcash_kernel::verify_key_path(
                    output_key, domain, signature,
                ))
            });
            (!opens).then(|| INVALID_K1.into())
        }
        Spend::ScriptPath { output_key, cw1 } => {
            // consensus accepts these unconditionally: the mint must not
            if let Some(reason) = check_leaf(cw1.control_block[0], &cw1.script) {
                return Some(reason.into());
            }
            let witness: Vec<&[u8]> = cw1.witness.iter().map(Vec::as_slice).collect();
            let opens = domains.iter().any(|domain| {
                core_says(lnurlcash_kernel::verify_script_path(
                    output_key,
                    domain,
                    &cw1.script,
                    &cw1.control_block,
                    &witness,
                    cw1.locktime,
                    cw1.sequence,
                ))
            });
            if !opens {
                // a cw1 discloses its whole secret already: its reason can't
                // help anyone guess
                return Some("bitcoin core rejected the spend".into());
            }
            check_time_claim(cw1.locktime, cw1.sequence, now, locked_at)
        }
        Spend::LegacyKeyPath { .. } => Some(INVALID_K1.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lnurlcash_core::recoverable::{
        derive_note_pubkey, derive_note_secret_key, encode_ck1, encode_cp1, encode_cw1,
        sign_note_ownership,
    };
    use lnurlcash_core::spend::bearer_cw1;
    use sha2::{Digest, Sha256};

    fn domains() -> Vec<String> {
        vec!["mint.example".into(), "abc.onion".into()]
    }

    #[test]
    fn bearer_short_forms_agree() {
        let preimage = [7u8; 32];
        let h = Sha256::digest(preimage);
        let parsed = parse(&hex::encode(preimage)).unwrap();
        assert_eq!(
            Some(parsed.note_id.clone()),
            note_id_of_ref(&hex::encode(h))
        );
        assert_eq!(verify(&parsed, 0, &domains(), 1), None);
        // the full cw1 names the same note and opens it too
        let cw1 = encode_cw1(&bearer_cw1(&preimage).unwrap()).unwrap();
        let full = parse(&cw1).unwrap();
        assert_eq!(full.note_id, parsed.note_id);
        assert_eq!(verify(&full, 0, &domains(), 1), None);
        let q: [u8; 32] = hex::decode(&parsed.note_id).unwrap().try_into().unwrap();
        assert_eq!(note_id_of_ref(&encode_cp1(&q)), Some(parsed.note_id));
    }

    #[test]
    fn key_path_binds_to_our_domains_only() {
        let secret = [3u8; 32];
        let ck1_here = encode_ck1(&sign_note_ownership(&secret, "abc.onion").unwrap());
        let ck1_elsewhere = encode_ck1(&sign_note_ownership(&secret, "other.example").unwrap());
        let here = parse(&ck1_here).unwrap();
        let elsewhere = parse(&ck1_elsewhere).unwrap();
        assert_eq!(here.note_id, elsewhere.note_id);
        assert_eq!(verify(&here, 0, &domains(), 1), None);
        assert_eq!(
            verify(&elsewhere, 0, &domains(), 1).as_deref(),
            Some(INVALID_K1)
        );
    }

    #[test]
    fn derived_keys_match_cx1_derivation() {
        let branch_sk = [9u8; 32];
        let chain = [1u8; 32];
        let kp = secp256k1::Keypair::from_secret_bytes(branch_sk).unwrap();
        let p = kp.x_only_public_key().0.to_byte_array();
        let sk = derive_note_secret_key(&branch_sk, &chain, 2, 5).unwrap();
        let q = derive_note_pubkey(&p, &chain, 2, 5).unwrap();
        let ck1 = encode_ck1(&sign_note_ownership(&sk, "mint.example").unwrap());
        assert_eq!(parse(&ck1).unwrap().note_id, hex::encode(q));
    }

    #[test]
    fn timelocks_use_the_mints_clock() {
        let preimage = [5u8; 32];
        let mut cw1 = bearer_cw1(&preimage).unwrap();
        cw1.locktime = 1_700_000_000;
        let parsed = parse(&encode_cw1(&cw1).unwrap()).unwrap();
        assert!(verify(&parsed, 0, &domains(), 1_600_000_000).is_some());
        assert_eq!(verify(&parsed, 0, &domains(), 1_800_000_000), None);
    }

    /// A leaf lnurlcash-core alone cannot judge: a hashlock behind a CLTV.
    #[test]
    fn core_judges_any_leaf() {
        use lnurlcash_core::recoverable::Cw1;
        use lnurlcash_core::spend::{NUMS_H, tapleaf_hash, taproot_tweak};

        let preimage = [9u8; 32];
        let mut script = vec![0x04];
        script.extend_from_slice(&1_700_000_000u32.to_le_bytes());
        script.extend_from_slice(&[0xb1, 0x75, 0xa8, 0x20]); // CLTV DROP SHA256 <32>
        script.extend_from_slice(&Sha256::digest(preimage));
        script.push(0x87); // EQUAL
        let tweak = taproot_tweak(&NUMS_H, &tapleaf_hash(&script, 0xc0)).unwrap();
        let mut control_block = vec![0xc0 | tweak.parity];
        control_block.extend_from_slice(&NUMS_H);
        let cw1 = Cw1 {
            locktime: 1_700_000_001,
            sequence: 0xffff_fffe,
            script,
            control_block,
            witness: vec![preimage.to_vec()],
        };
        let parsed = parse(&encode_cw1(&cw1).unwrap()).unwrap();
        assert_eq!(parsed.note_id, hex::encode(tweak.output_key));
        // Core accepts it; the mint's clock decides when
        assert_eq!(verify(&parsed, 0, &domains(), 1_800_000_000), None);
        assert!(verify(&parsed, 0, &domains(), 1_600_000_000).is_some());
        // a locktime below the script's is Core's to refuse
        let early = Cw1 {
            locktime: 1_600_000_000,
            ..cw1.clone()
        };
        let parsed = parse(&encode_cw1(&early).unwrap()).unwrap();
        assert_eq!(
            verify(&parsed, 0, &domains(), 1_800_000_000).as_deref(),
            Some("bitcoin core rejected the spend")
        );
        // a wrong preimage too
        let wrong = Cw1 {
            witness: vec![vec![1; 32]],
            ..cw1
        };
        let parsed = parse(&encode_cw1(&wrong).unwrap()).unwrap();
        assert!(verify(&parsed, 0, &domains(), 1_800_000_000).is_some());
    }

    #[test]
    fn upgrade_hooks_are_refused_before_core_sees_them() {
        use lnurlcash_core::recoverable::Cw1;
        use lnurlcash_core::spend::{NUMS_H, tapleaf_hash, taproot_tweak};

        // OP_SUCCESS80: consensus would accept it unconditionally
        let script = vec![0x50];
        let tweak = taproot_tweak(&NUMS_H, &tapleaf_hash(&script, 0xc0)).unwrap();
        let mut control_block = vec![0xc0 | tweak.parity];
        control_block.extend_from_slice(&NUMS_H);
        let cw1 = Cw1 {
            locktime: 0,
            sequence: 0xffff_ffff,
            script,
            control_block,
            witness: vec![],
        };
        let parsed = parse(&encode_cw1(&cw1).unwrap()).unwrap();
        assert_eq!(
            verify(&parsed, 0, &domains(), 1).as_deref(),
            Some("leaf uses a reserved OP_SUCCESS opcode")
        );
    }

    #[test]
    fn garbage_is_no_spend() {
        assert!(parse("hello").is_none());
        assert!(parse("").is_none());
    }
}
