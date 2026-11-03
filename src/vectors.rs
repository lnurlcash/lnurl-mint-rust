//! LUD-25 and LUD-26's own test vectors, run against this plugin's paths.

use lnurlcash_core::{
    recoverable::{decode_cx1, derive_note_pubkey, encode_cs1_with_amount},
    signature::note_signature_message_for_hash,
};

use crate::mint::{Share, proves, share_note_id};
use serde_json::Value;

use crate::spend;

fn vectors(name: &str) -> Value {
    let raw = match name {
        "25" => include_str!("../tests/vectors/25-vectors.json"),
        _ => include_str!("../tests/vectors/26-vectors.json"),
    };
    serde_json::from_str(raw).unwrap()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("vector field {key}"))
}

#[test]
fn key_path_spend_opens_its_note_at_its_domain_only() {
    let v = &vectors("25")["key_path_spend"];
    let parsed = spend::parse(s(v, "ck1")).unwrap();
    assert_eq!(parsed.note_id, s(v, "Q"));
    assert_eq!(
        spend::note_id_of_ref(s(v, "cp1")).as_deref(),
        Some(s(v, "Q"))
    );
    assert_eq!(spend::verify(&parsed, 0, &[s(v, "domain").into()], 1), None);
    assert!(spend::verify(&parsed, 0, &["other.example".into()], 1).is_some());
}

#[test]
fn certificates_encode_as_the_vectors_do() {
    let v = &vectors("25")["certificates"];
    for case in v["cases"].as_array().unwrap() {
        let amount = case["amount_msat"].as_u64().unwrap();
        assert_eq!(
            note_signature_message_for_hash(s(v, "Q"), s(v, "domain"), amount).as_deref(),
            Some(s(case, "message"))
        );
        let sig: [u8; 65] = hex::decode(s(case, "signature"))
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(encode_cs1_with_amount(amount, &sig), s(case, "cs1"));
    }
}

#[test]
fn bearer_note_every_form_names_one_note() {
    let v = &vectors("25")["bearer_note"];
    let q = s(v, "Q");
    for reference in [s(v, "h"), s(v, "cp1")] {
        assert_eq!(spend::note_id_of_ref(reference).as_deref(), Some(q));
    }
    for k1 in [s(v, "preimage"), s(v, "cw1")] {
        let parsed = spend::parse(k1).unwrap();
        assert_eq!(parsed.note_id, q);
        // a bearer leaf checks no signature: any domain
        assert_eq!(
            spend::verify(&parsed, 0, &["anywhere.example".into()], 1),
            None
        );
    }
    let cert = &v["certificate"];
    let sig: [u8; 65] = hex::decode(s(cert, "signature"))
        .unwrap()
        .try_into()
        .unwrap();
    let amount = cert["amount_msat"].as_u64().unwrap();
    assert_eq!(encode_cs1_with_amount(amount, &sig), s(cert, "cs1"));
}

#[test]
fn lud26_key_path_spend_opens_its_odd_y_note() {
    let v = &vectors("26")["key_path_spend"];
    let parsed = spend::parse(s(v, "ck1")).unwrap();
    assert_eq!(parsed.note_id, s(v, "Q"));
    assert_eq!(spend::verify(&parsed, 0, &[s(v, "domain").into()], 1), None);
}

/// What this mint credits a registered share with: every share note in the
/// vectors, from its cx1 alone, the way the share is kept on file.
#[test]
fn a_share_on_file_derives_every_share_note() {
    for derivation in vectors("26")["derivation"].as_array().unwrap() {
        for account in derivation["accounts"].as_array().unwrap() {
            for share in account["shareable"].as_array().unwrap() {
                let cx1 = decode_cx1(s(share, "cx1")).unwrap();
                let on_file = hex::encode([&cx1.chain_code[..], &cx1.pubkey[..]].concat());
                assert_eq!(Share::on_file(&on_file).unwrap().cx1(), s(share, "cx1"));
                for note in share["notes"].as_array().unwrap() {
                    let i = note["i"].as_u64().unwrap() as u32;
                    let q = derive_note_pubkey(&cx1, i).unwrap();
                    assert_eq!(hex::encode(q), s(note, "Q"));
                    assert_eq!(share_note_id(&on_file, i), Some(hex::encode(q)));
                    assert_eq!(spend::note_id_of_ref(s(note, "cp1")), Some(hex::encode(q)));
                }
            }
        }
    }
}

#[test]
fn registration_proofs_verify() {
    let v = vectors("26");
    let reg = &v["registration"];
    let (domain, username) = (s(reg, "domain"), s(reg, "username"));
    let (cx1, new_cx1) = (s(reg, "cx1"), s(reg, "new_cx1"));
    let xp = |value: &str| decode_cx1(value).unwrap().pubkey_x_only();
    for (proof, action, bound, signer) in [
        (&reg["register"], "register", Some(cx1), cx1),
        (&reg["update"]["sig"], "update", Some(new_cx1), cx1),
        (&reg["update"]["newsig"], "register", Some(new_cx1), new_cx1),
        (&reg["unregister"], "unregister", None, new_cx1),
    ] {
        let sig = s(proof, "sig");
        assert!(
            proves(action, domain, username, bound, xp(signer), sig),
            "{action}"
        );
        // by any other key, or for another action, it proves nothing
        let other = if signer == cx1 { new_cx1 } else { cx1 };
        assert!(
            !proves(action, domain, username, bound, xp(other), sig),
            "{action}"
        );
        assert!(
            !proves("delete", domain, username, bound, xp(signer), sig),
            "{action}"
        );
    }
}
