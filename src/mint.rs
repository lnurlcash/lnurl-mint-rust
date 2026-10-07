//! LUD-25 and LUD-26, as lnurl-mint implements them: minting from a
//! payRequest, the informational withdrawRequest, melt, rotate, split and
//! merge at its callback, offline-verification certificates, and Lightning
//! Address auto-mint onto a registered `cx1` branch.
//!
//! Every function here answers in LUD-01's shape: success JSON, or a
//! [`MintError`] the HTTP layer turns into `{"status": "ERROR", "reason"}`.

use lnurlcash_core::{
    recoverable::{
        PURPOSE_LIGHTNING_ADDRESS, PURPOSE_WALLET, decode_cx1, derive_note_pubkey,
        encode_cs1_with_amount, encode_cx1,
    },
    signature::{address_proof_digest, note_signature_message_for_hash},
};
use secp256k1::{XOnlyPublicKey, schnorr};
use serde_json::{Map, Value, json};

use crate::{
    db::{self, StoreError},
    ln::{PayError, PayStatus},
    spend,
    state::AppState,
};

#[derive(Debug)]
pub enum MintError {
    /// A refusal whose reason is safe to hand back.
    Reject(String),
    /// Something broke. Its detail is logged, never sent.
    Internal(anyhow::Error),
}

pub type MintResult<T> = Result<T, MintError>;

fn reject<T>(reason: impl Into<String>) -> MintResult<T> {
    Err(MintError::Reject(reason.into()))
}

impl From<StoreError> for MintError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Sql(e) => MintError::Internal(e.into()),
            other => MintError::Reject(other.to_string()),
        }
    }
}

impl MintError {
    /// The reason to send. An internal error gets a reference id instead of
    /// its text, which is logged alongside it.
    pub fn reason(self) -> String {
        match self {
            MintError::Reject(reason) => reason,
            MintError::Internal(err) => {
                let reference = hex::encode(rand::random::<[u8; 4]>());
                log::warn!("[{reference}] internal error: {err:#}");
                format!("Internal error (reference: {reference}).")
            }
        }
    }
}

/// Drop `null` fields: LUD responses omit what they do not have.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

/// A registrable username: short, lowercase, and never shaped like a note.
pub fn valid_username(username: &str) -> bool {
    (1..=32).contains(&username.len())
        && username
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

fn decode_npub(npub: &str) -> Option<[u8; 32]> {
    use bech32::{Bech32, primitives::decode::CheckedHrpstring};
    // NIP-19 is classic bech32, never bech32m
    let checked = CheckedHrpstring::new::<Bech32>(npub).ok()?;
    if checked.hrp().to_lowercase() != "npub" {
        return None;
    }
    checked.byte_iter().collect::<Vec<u8>>().try_into().ok()
}

/// A cx1 as stored: `hex(P || chain_code)`.
fn branch_parts(branch_hex: &str) -> Option<([u8; 32], [u8; 32])> {
    let bytes = hex::decode(branch_hex).ok()?;
    if bytes.len() != 64 {
        return None;
    }
    Some((bytes[..32].try_into().ok()?, bytes[32..].try_into().ok()?))
}

/// Does `sig` prove control of the branch's purpose-0 index-0 key, over
/// `sha256("LNURLcash:<action>:<domain>:<username>")`?
pub(crate) fn owns_branch(
    action: &str,
    domain: &str,
    username: &str,
    branch_hex: &str,
    sig: &str,
) -> bool {
    let Some((point, chain_code)) = branch_parts(branch_hex) else {
        return false;
    };
    let Ok(pk0) = derive_note_pubkey(&point, &chain_code, PURPOSE_WALLET, 0) else {
        return false;
    };
    let Ok(digest) = address_proof_digest(action, domain, username) else {
        return false;
    };
    let Some(sig) = hex::decode(sig.trim())
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
    else {
        return false;
    };
    let Ok(pk) = XOnlyPublicKey::from_byte_array(pk0) else {
        return false;
    };
    schnorr::verify(&schnorr::Signature::from_byte_array(sig), &digest, &pk).is_ok()
}

impl AppState {
    fn is_own_username(&self, username: &str) -> bool {
        let username = username.to_ascii_lowercase();
        username == "_" || username == self.settings.username.to_ascii_lowercase()
    }

    /// A registered username's branch, while registration is enabled.
    fn registered_branch(&self, username: &str) -> MintResult<Option<String>> {
        if !self.settings.username_registration_enabled {
            return Ok(None);
        }
        Ok(self.store.username_branch(&username.to_ascii_lowercase())?)
    }

    pub fn mint_pubkey(&self) -> Option<String> {
        self.ln.node_id()
    }

    /// A `cs1` certificate over `(Q, amount)`, signed by this node. `None` if
    /// signing failed: the mutation it belongs to still stands.
    fn certificate(&self, note_id: &str, amount_msat: u64) -> Option<String> {
        let message = note_signature_message_for_hash(note_id, amount_msat);
        match self.ln.sign_message(&message) {
            Ok(sig) => Some(encode_cs1_with_amount(amount_msat, &sig)),
            Err(e) => {
                log::debug!("could not certify a note: {e:#}");
                None
            }
        }
    }

    // ---- payRequest (minting) ----

    /// The metadata a username's payRequest advertises, and its invoices commit to.
    fn pay_metadata(&self, username: &str, host: &str) -> MintResult<String> {
        let mut entries = vec![
            json!([
                "text/plain",
                format!("Mint an lnurlcash bearer note on {host}")
            ]),
            json!(["text/identifier", format!("{username}@{host}")]),
        ];
        if !self.is_own_username(username) {
            if let Some(branch_hex) = self.registered_branch(username)? {
                let (point, chain_code) = branch_parts(&branch_hex)
                    .ok_or_else(|| MintError::Internal(anyhow::anyhow!("corrupt cx1 on file")))?;
                let hint = self.store.next_index_hint(username)?.unwrap_or(0);
                entries.push(json!([
                    "text/cpub",
                    format!("{}:{hint}", encode_cx1(&point, &chain_code))
                ]));
            }
        }
        if self.settings.has_fee() {
            entries.push(json!([
                "text/plain",
                format!(
                    "Mint fees: {},{}",
                    self.settings.base_fee_msat, self.settings.fee_percent_ppm
                )
            ]));
        }
        Ok(Value::Array(entries).to_string())
    }

    /// `GET /.well-known/lnurlp/<username>`: LUD-16 payRequest that mints a note.
    pub fn pay_request(&self, username: &str, request_host: Option<&str>) -> MintResult<Value> {
        let username = username.to_ascii_lowercase();
        let own = self.is_own_username(&username);
        if !own && self.registered_branch(&username)?.is_none() {
            return reject("Unknown user.");
        }
        let (base, host) = self.settings.public_base_url_and_host(request_host);
        // the fixed identity always names itself by its canonical username, so
        // its callback can rebuild the same metadata the invoice commits to
        let name = if own {
            self.settings.username.to_ascii_lowercase()
        } else {
            username.clone()
        };
        let callback = if own {
            format!("{base}/p/cb")
        } else {
            format!("{base}/p/{name}")
        };
        Ok(compact(json!({
            "tag": "payRequest",
            "callback": callback,
            "minSendable": self.settings.min_sendable(),
            "maxSendable": self.settings.max_sendable_msat,
            "metadata": self.pay_metadata(&name, &host)?,
            "withdrawLink": format!("{base}/w"),
            // a registered address always mints onto its own branch
            "commentAllowed": if own { Some(64) } else { None },
        })))
    }

    /// `GET /p/cb` and `GET /p/<username>`: an invoice that, once paid, credits
    /// the note named by `comment` (fixed identity) or the next key on the
    /// username's branch (LUD-26 auto-mint).
    pub fn pay_callback(
        &self,
        username: Option<&str>,
        amount: u64,
        comment: Option<&str>,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        let s = &self.settings;
        if s.sunset_mint {
            return reject("This mint is sunsetting - minting is disabled.");
        }
        let username = username.map(str::to_ascii_lowercase);
        let branch = match &username {
            Some(name) => match self.registered_branch(name)? {
                Some(branch) => Some(branch),
                None => return reject("Unknown user."),
            },
            None => None,
        };
        if amount < s.min_sendable_msat {
            return reject("Amount too low.");
        }
        if amount > s.max_sendable_msat {
            return reject("Amount too high.");
        }
        let net = amount - s.mint_fee_msat(amount).min(amount);
        if net < s.min_mint_msat {
            return reject(format!(
                "Amount too low to mint a note (min {} msat net of fees).",
                s.min_mint_msat
            ));
        }
        let (base, host) = s.public_base_url_and_host(request_host);
        let name = username
            .clone()
            .unwrap_or_else(|| s.username.to_ascii_lowercase());
        let metadata = self.pay_metadata(&name, &host)?;

        let note_id = match (&branch, &username) {
            (Some(branch_hex), Some(name)) => {
                let (point, chain_code) = branch_parts(branch_hex)
                    .ok_or_else(|| MintError::Internal(anyhow::anyhow!("corrupt cx1 on file")))?;
                self.store
                    .claim_next_index(name, |i| {
                        derive_note_pubkey(&point, &chain_code, PURPOSE_LIGHTNING_ADDRESS, i)
                            .ok()
                            .map(hex::encode)
                    })?
                    .0
            }
            _ => match comment.and_then(spend::note_id_of_ref) {
                Some(id) => id,
                None => {
                    return reject(
                        "Missing or malformed comment: a cp1<Q>, or a bearer note's \
                         hex-encoded 32-byte hash, is required to mint.",
                    );
                }
            },
        };
        // refuse a taken output before an invoice exists for it
        if self.store.note_record(&note_id)?.is_some()
            || self.store.pending_mint_by_note_id(&note_id)?.is_some()
        {
            return reject("already in use");
        }
        if let Err(e) = self.ln.ready() {
            log::warn!("refusing a mint: {e:#}");
            return reject("Minting is temporarily unavailable.");
        }
        let invoice = self
            .ln
            .create_invoice(amount, &metadata, &note_id)
            .map_err(MintError::Internal)?;
        self.store
            .create_mint(&invoice.payment_hash, &invoice.bolt11, net, &note_id)?;
        Ok(compact(json!({
            "pr": invoice.bolt11,
            "routes": [],
            "disposable": false,
            "verify": s.verify_enabled.then(|| format!("{base}/verify/{}", invoice.payment_hash)),
        })))
    }

    /// `GET /verify/<payment_hash>`: LUD-21, for a mint invoice or a melt.
    /// Settlement is what the store recorded from the node's own events; a
    /// mint's preimage is recomputed by the node, never stored.
    pub fn verify(&self, payment_hash: &str) -> MintResult<Value> {
        if !self.settings.verify_enabled {
            return reject("Not found");
        }
        if let Some(pr) = self.store.mint_pr(payment_hash)? {
            let settled = self.store.mint_settled(payment_hash)?;
            return Ok(compact(json!({
                "status": "OK",
                "settled": settled,
                "preimage": match self.store.mint_note_id(payment_hash)? {
                    Some(note_id) if settled => Some(hex::encode(self.ln.mint_preimage(&note_id))),
                    _ => None,
                },
                "pr": pr,
            })));
        }
        if let Some(pr) = self.store.melt_pr(payment_hash)? {
            let settled = self.store.melt_settled(payment_hash)?;
            return Ok(compact(json!({
                "status": "OK",
                "settled": settled,
                "preimage": if settled { self.store.melt_preimage(payment_hash)? } else { None },
                "pr": pr,
            })));
        }
        reject("Not found")
    }

    // ---- withdrawRequest (redeeming) ----

    /// The note `k1` spends and its record, once the spend is verified to
    /// open it. Spent notes are returned too, so a retried burn is recognised.
    fn verified_note(&self, k1: &str) -> MintResult<(String, db::NoteRecord)> {
        let Some(parsed) = spend::parse(k1) else {
            return reject(spend::INVALID_K1);
        };
        let Some(record) = self.store.note_record(&parsed.note_id)? else {
            return reject(spend::INVALID_K1);
        };
        if let Some(reason) = spend::verify(
            &parsed,
            record.locked_at,
            &self.settings.spend_domains(),
            db::now(),
        ) {
            return reject(reason);
        }
        Ok((parsed.note_id, record))
    }

    /// `GET /w?k1=<spend>` or `GET /w?p=<cp1>`: read-only; the note's value.
    pub fn withdraw_request(
        &self,
        k1: Option<&str>,
        p: Option<&str>,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        let (note_id, record) = match (k1, p) {
            (Some(k1), None) => self.verified_note(k1).map_err(|e| match e {
                MintError::Reject(r) if r == spend::INVALID_K1 => {
                    MintError::Reject("Unknown note.".into())
                }
                other => other,
            })?,
            (None, Some(p)) => {
                let Some(note_id) = spend::note_id_of_ref(p) else {
                    return reject("Unknown note.");
                };
                match self.store.note_record(&note_id)? {
                    Some(record) => (note_id, record),
                    None => return reject("Unknown note."),
                }
            }
            _ => return reject("Specify exactly one of k1 or p."),
        };
        if record.spent {
            return reject("Note already spent.");
        }
        // a note mid-melt must not be advertised as withdrawable
        if record.pending {
            return reject("pending");
        }
        let (base, host) = self.settings.public_base_url_and_host(request_host);
        Ok(compact(json!({
            "tag": "withdrawRequest",
            "callback": format!("{base}/w/cb"),
            "k1": k1,
            "minWithdrawable": record.amount_msat,
            "maxWithdrawable": record.amount_msat,
            "defaultDescription": format!("lnurlcash bearer note on {host}"),
            "mintPubkey": self.mint_pubkey(),
            "c": self.certificate(&note_id, record.amount_msat),
        })))
    }

    /// `GET /w/cb`: melt, rotate, split or merge (LUD-25's callback table).
    pub fn withdraw_callback(
        &self,
        k1s: &[String],
        pr: Option<&str>,
        amount: Option<u64>,
        p1: Option<&str>,
        p2: Option<&str>,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        let s = &self.settings;
        if k1s.is_empty() {
            return reject("missing k1");
        }
        if k1s.len() > s.max_k1s {
            return reject("too many k1");
        }
        if pr.is_some() && (k1s.len() > 1 || amount.is_some()) {
            return reject(
                "pr cannot be combined with multiple k1s or amount - merge or split first.",
            );
        }
        if s.sunset_mint && amount.is_some() {
            return reject("This mint is sunsetting - splitting is disabled.");
        }
        // outputs are validated before any note is touched
        let (mut p1_id, mut p2_id) = (None, None);
        if pr.is_none() {
            p1_id = match p1.and_then(spend::note_id_of_ref) {
                Some(id) => Some(id),
                None => return reject("missing p1"),
            };
            if amount.is_some() {
                p2_id = match p2.and_then(spend::note_id_of_ref) {
                    Some(id) => Some(id),
                    None => return reject("missing p2"),
                };
            }
        }

        // every spend must open its note, or nothing happens
        let mut notes = Vec::with_capacity(k1s.len());
        for k1 in k1s {
            notes.push(self.verified_note(k1)?);
        }
        let note_ids: Vec<String> = notes.iter().map(|(id, _)| id.clone()).collect();

        if pr.is_none() {
            // a retried rotate/split/merge gets its original answer
            if let Some(burn) = self.store.find_burn(&note_ids)? {
                let recorded_amount = burn.id2.as_ref().map(|_| burn.amount1_msat);
                if Some(&burn.id) == p1_id.as_ref()
                    && burn.id2 == p2_id
                    && recorded_amount == amount
                {
                    let c2 = match (&burn.id2, burn.amount2_msat) {
                        (Some(id2), Some(a2)) => self.certificate(id2, a2),
                        _ => None,
                    };
                    return Ok(compact(json!({
                        "status": "OK",
                        "c": self.certificate(&burn.id, burn.amount1_msat),
                        "c2": c2,
                    })));
                }
            }
        }
        if notes.iter().any(|(_, r)| r.spent) {
            return reject(spend::INVALID_K1);
        }
        if notes.iter().any(|(_, r)| r.pending) {
            return reject("pending");
        }
        let total: u64 = notes.iter().map(|(_, r)| r.amount_msat).sum();

        if let Some(pr) = pr {
            return self.melt(note_ids, pr, total, request_host);
        }
        let p1_id = p1_id.expect("validated above");

        if let Some(amount) = amount {
            if amount == 0 || amount >= total {
                return reject(format!("amount must be between 0 and {total} msat."));
            }
            // the base fee comes out of the change, never the requested amount
            let change = (total - amount)
                .checked_sub(s.base_fee_msat)
                .filter(|c| *c >= 1);
            let Some(change) = change else {
                return reject("insufficient value");
            };
            let p2_id = p2_id.expect("validated above");
            self.store.swap(
                &note_ids,
                &[(p1_id.clone(), amount), (p2_id.clone(), change)],
            )?;
            return Ok(compact(json!({
                "status": "OK",
                "c": self.certificate(&p1_id, amount),
                "c2": self.certificate(&p2_id, change),
            })));
        }

        // rotate is a merge of one: the refund is 0
        let merged = total + (note_ids.len() as u64 - 1) * s.base_fee_msat;
        self.store.swap(&note_ids, &[(p1_id.clone(), merged)])?;
        Ok(compact(json!({
            "status": "OK",
            "c": self.certificate(&p1_id, merged),
        })))
    }

    /// Reserve the notes and start paying `pr` (LUD-03 answers before the
    /// payment settles). The node's events burn the notes once it settles,
    /// or release them once it provably failed (`ln/events.rs`).
    fn melt(
        &self,
        note_ids: Vec<String>,
        pr: &str,
        total: u64,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        if let Err(e) = self.ln.ready() {
            log::warn!("refusing a melt: {e:#}");
            return reject("Melting is temporarily unavailable.");
        }
        let decoded = match self.ln.decode_invoice(pr) {
            Ok(d) => d,
            Err(e) => return reject(format!("Invalid invoice: {e}")),
        };
        if decoded.amount_msat != Some(total) {
            return reject(format!("Invoice must be for exactly {total} msat."));
        }
        let hash = decoded.payment_hash;
        if self.store.mint_pr(&hash)?.is_some()
            || self.ln.node_id().as_ref() == Some(&decoded.payee)
        {
            return reject("Cannot melt into an invoice this mint issued itself.");
        }
        // payments are keyed by hash: a second melt into the same invoice
        // would confirm against the first payment and burn for free
        if self.store.melt_pr(&hash)?.is_some() {
            return reject("Invoice already used by an earlier melt - use a fresh one.");
        }
        self.store.mark_pending(&note_ids, &hash)?;
        if let Err(e) = self.store.record_melt(&hash, pr) {
            // nothing was paid: release the reservation
            let _ = self.store.restore_melt(&hash);
            return Err(e.into());
        }
        // reconciliation must not mistake a payment being handed to the node
        // right now for one that never left
        self.track_melt(&hash, true);
        let sent = self.ln.pay(pr, self.settings.melt_fee_limit_msat(total));
        self.track_melt(&hash, false);
        match sent {
            Ok(()) | Err(PayError::InFlight) => {}
            Err(PayError::NotSent(reason)) => {
                self.store.abort_melt(&hash)?;
                log::info!("melt {hash}: not sent ({reason}) - restored {note_ids:?}");
                return reject(reason);
            }
        }
        let (base, _) = self.settings.public_base_url_and_host(request_host);
        Ok(compact(json!({
            "status": "OK",
            "pr": self.settings.verify_enabled.then_some(pr),
            "verify": self.settings.verify_enabled.then(|| format!("{base}/verify/{hash}")),
        })))
    }

    fn track_melt(&self, payment_hash: &str, start: bool) {
        let mut live = self
            .in_flight_melts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let count = live.entry(payment_hash.to_string()).or_insert(0);
        if start {
            *count += 1;
        } else {
            *count = count.saturating_sub(1);
            if *count == 0 {
                live.remove(payment_hash);
            }
        }
    }

    /// What the notes reserved by a melt are worth: its invoice's amount.
    fn melt_total(&self, note_ids: &[String]) -> MintResult<u64> {
        let mut total = 0;
        for id in note_ids {
            total += self.store.note_record(id)?.map_or(0, |r| r.amount_msat);
        }
        Ok(total)
    }

    fn melt_in_flight(&self, payment_hash: &str) -> bool {
        self.in_flight_melts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(payment_hash)
    }

    /// Resolve melts whose outcome no event resolved: a crash between
    /// reserving the notes and handing the payment to the node, or an event
    /// lost to an earlier version. Returns what was done, by payment hash.
    ///
    /// The node's payment store decides: a success burns the notes, a failure
    /// releases them, a pending payment is left to its event. A payment the
    /// store never heard of is sent again, its original invoice, once a
    /// channel is usable: LDK keys it by payment hash, so it can never go out
    /// twice. A resend refused before anything left is recorded as failed by
    /// the node, and released on the next round.
    pub fn reconcile_pending_melts(&self) -> MintResult<Value> {
        let mut report = Map::new();
        for (hash, note_ids) in self.store.pending_melts()? {
            if self.melt_in_flight(&hash) {
                report.insert(hash, json!("in flight"));
                continue;
            }
            let outcome = match self.ln.payment_status(&hash) {
                Ok(PayStatus::Complete) => {
                    let (burned, amount) = self.store.finalize_melt(&hash, None)?;
                    log::info!(
                        "MELT payment_hash={hash} notes={burned:?} amount_msat={amount} (reconciled)"
                    );
                    "finalized"
                }
                Ok(PayStatus::Failed) => {
                    self.store.restore_melt(&hash)?;
                    log::info!("reconcile: melt {hash} failed - restored {note_ids:?}");
                    "restored"
                }
                // still reconnecting: a send now could only fail
                Ok(PayStatus::Absent) if !self.ln.has_usable_channel() => "waiting for a channel",
                Ok(PayStatus::Absent) => match self.store.melt_pr(&hash)? {
                    Some(pr) => {
                        let total = self.melt_total(&note_ids)?;
                        match self.ln.pay(&pr, self.settings.melt_fee_limit_msat(total)) {
                            Ok(()) | Err(PayError::InFlight) => {
                                log::info!("reconcile: melt {hash} was never sent - sent it now");
                                "resent"
                            }
                            Err(PayError::NotSent(reason)) => {
                                log::warn!(
                                    "reconcile: melt {hash} could not be sent ({reason}); the node \
                                     recorded it failed, its notes are released next round"
                                );
                                "unsent"
                            }
                        }
                    }
                    None => {
                        log::warn!("reconcile: melt {hash} has no invoice on record");
                        "unknown"
                    }
                },
                Ok(PayStatus::Pending) => "pending",
                Err(e) => {
                    log::warn!("reconcile: melt {hash}: {e:#}");
                    "unknown"
                }
            };
            report.insert(hash, json!(outcome));
        }
        Ok(Value::Object(report))
    }

    // ---- Lightning Address registration (LUD-26) ----

    /// `POST /p/<username>?cx1=..&sig=..[&npub=..]`: claim or overwrite.
    pub fn register(
        &self,
        username: &str,
        cx1: &str,
        sig: &str,
        npub: Option<&str>,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        if !self.settings.username_registration_enabled {
            return reject("Not found");
        }
        let username = username.to_ascii_lowercase();
        if !valid_username(&username) || self.is_own_username(&username) {
            return reject("Invalid or reserved username.");
        }
        let Some(branch) = decode_cx1(cx1) else {
            return reject("Invalid cx1.");
        };
        let branch_hex = hex::encode([branch.pubkey_x_only, branch.chain_code].concat());
        let nostr_pubkey = match npub {
            None => None,
            Some(_) if !self.settings.nip05_enabled => {
                return reject("npub registration (NIP-05) is disabled on this mint.");
            }
            Some(npub) => match decode_npub(npub) {
                Some(key) => Some(hex::encode(key)),
                None => return reject("Invalid npub."),
            },
        };
        // a fresh claim proves the new branch; an overwrite proves the one on file
        let proof_branch = self
            .store
            .username_branch(&username)?
            .unwrap_or_else(|| branch_hex.clone());
        let (_, host) = self.settings.public_base_url_and_host(request_host);
        if !owns_branch("register", &host, &username, &proof_branch, sig) {
            return reject("Invalid ownership signature.");
        }
        self.store
            .upsert_username(&username, &branch_hex, nostr_pubkey.as_deref())?;
        Ok(json!({"status": "OK"}))
    }

    /// `DELETE /p/<username>?sig=..`: free a username, proven by the branch on file.
    pub fn unregister(
        &self,
        username: &str,
        sig: &str,
        request_host: Option<&str>,
    ) -> MintResult<Value> {
        if !self.settings.username_registration_enabled {
            return reject("Not found");
        }
        let username = username.to_ascii_lowercase();
        let Some(branch_hex) = self.store.username_branch(&username)? else {
            return reject("Unknown user.");
        };
        let (_, host) = self.settings.public_base_url_and_host(request_host);
        if !owns_branch("unregister", &host, &username, &branch_hex, sig) {
            return reject("Invalid ownership signature.");
        }
        self.store.delete_username(&username)?;
        Ok(json!({"status": "OK"}))
    }

    /// `GET /.well-known/nostr.json?name=..`: NIP-05 for registered usernames.
    pub fn nip05(&self, name: Option<&str>) -> MintResult<Value> {
        if !self.settings.nip05_enabled {
            return reject("Not found");
        }
        let mut names = Map::new();
        if let (Some(name), true) = (name, self.settings.username_registration_enabled) {
            if let Some(pubkey) = self.store.nostr_pubkey(&name.to_ascii_lowercase())? {
                names.insert(name.to_string(), json!(pubkey));
            }
        }
        Ok(json!({"names": names}))
    }

    /// `GET /.well-known/lnurlw/<username>`: informational only. Describes
    /// this mint and points back at its payRequest; there is no balance here.
    pub fn mint_address(&self, username: &str, request_host: Option<&str>) -> MintResult<Value> {
        let username = username.to_ascii_lowercase();
        let name = if self.is_own_username(&username) {
            self.settings.username.to_ascii_lowercase()
        } else if self.registered_branch(&username)?.is_some() {
            username
        } else {
            return reject("Unknown user.");
        };
        let (base, host) = self.settings.public_base_url_and_host(request_host);
        let info = self.ln.info().ok();
        Ok(compact(json!({
            "tag": "withdrawRequest",
            "callback": format!("{base}/w"),
            "minWithdrawable": self.settings.min_mint_msat,
            "maxWithdrawable": self.settings.max_mintable(),
            "defaultDescription": format!("lnurlcash bearer note on {host}"),
            "mintPubkey": info.as_ref().map(|i| i.id.clone()),
            "payLink": format!("{base}/.well-known/lnurlp/{name}"),
            "nodeAlias": info.as_ref().and_then(|i| i.alias.clone()),
            "nodeUri": info.as_ref().and_then(|i| i.uris.first().cloned()),
            "nodeUris": info.as_ref().filter(|i| !i.uris.is_empty()).map(|i| i.uris.clone()),
            "nodeColor": info.as_ref().and_then(|i| i.color.clone()),
            "nodeCapacity": info.as_ref().and_then(|i| i.capacity_msat),
            "nodeNumChannels": info.as_ref().and_then(|i| i.num_channels),
            "nodeNumPeers": info.as_ref().and_then(|i| i.num_peers),
            "sunsetDate": self.settings.sunset_date,
            "outstandingNotesMsat": self.store.stats()?.outstanding_msat,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lnurlcash_core::{
        recoverable::{derive_note_secret_key, encode_cx1},
        signature::sign_address_proof,
    };

    #[test]
    fn usernames() {
        assert!(valid_username("alice.b-c_1"));
        assert!(!valid_username("Alice"));
        assert!(!valid_username(""));
        assert!(!valid_username(&"a".repeat(33)));
    }

    #[test]
    fn registration_proofs_bind_action_domain_and_username() {
        let branch_sk = [11u8; 32];
        let chain_code = [2u8; 32];
        let kp = secp256k1::Keypair::from_secret_bytes(branch_sk).unwrap();
        let point = kp.x_only_public_key().0.to_byte_array();
        let branch_hex = hex::encode([point, chain_code].concat());
        let sk0 = derive_note_secret_key(&branch_sk, &chain_code, PURPOSE_WALLET, 0).unwrap();
        let sig =
            hex::encode(sign_address_proof(&sk0, "register", "mint.example", "alice").unwrap());
        assert!(owns_branch(
            "register",
            "mint.example",
            "alice",
            &branch_hex,
            &sig
        ));
        assert!(!owns_branch(
            "unregister",
            "mint.example",
            "alice",
            &branch_hex,
            &sig
        ));
        assert!(!owns_branch(
            "register",
            "other.example",
            "alice",
            &branch_hex,
            &sig
        ));
        assert!(!owns_branch(
            "register",
            "mint.example",
            "bob",
            &branch_hex,
            &sig
        ));
        assert!(decode_cx1(&encode_cx1(&point, &chain_code)).is_some());
    }

    #[test]
    fn npub_decodes() {
        // NIP-19's own example
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        assert_eq!(
            decode_npub(npub).map(hex::encode).as_deref(),
            Some("7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e")
        );
        assert!(decode_npub("npub1xyz").is_none());
    }
}
