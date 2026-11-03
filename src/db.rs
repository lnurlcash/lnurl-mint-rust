//! The note store: outstanding notes, pending mints, melts, burns and
//! registered usernames.
//!
//! No spend is ever persisted. A note is its taproot output key `Q` and is
//! stored under `hex(Q)`, the `cp1` a WALLET disclosed. A leaked database
//! reveals how many notes are outstanding and for how much, but lets nobody
//! spend them.
//!
//! The schema is lnurl-mint's (and cln-mint's), so a database can move between
//! them.
//! Burned notes are kept with `spent = 1` rather than deleted, so a replayed
//! spend fails as "already spent" and a burned `Q` is never reissued.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

pub const INVALID_K1: &str = "Invalid or already spent k1.";

#[derive(Debug)]
pub enum StoreError {
    /// A note being burned or melted is reserved by an in-flight melt.
    Pending,
    /// An output (`p1`, `p2`, a mint comment) names a note already in use.
    InUse,
    /// A note is unknown, already spent, or named twice.
    Invalid,
    /// A POST for a username that is already registered (LUD-26).
    AlreadyRegistered,
    /// A share whose `x(P)` this mint has accepted before (LUD-26).
    KeyUsed,
    /// A username nobody has registered.
    UnknownUser,
    /// An address fee invoice that is paid already, expired, or not for the
    /// amount offered (LUD-26).
    FeeNotPayable,
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        StoreError::Sql(err)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Pending => f.write_str("pending"),
            StoreError::InUse => f.write_str("already in use"),
            StoreError::Invalid => f.write_str(INVALID_K1),
            StoreError::AlreadyRegistered => f.write_str("already registered"),
            StoreError::KeyUsed => f.write_str("cx1 already used"),
            StoreError::UnknownUser => f.write_str("unknown user"),
            StoreError::FeeNotPayable => f.write_str("address fee invoice already paid or expired"),
            StoreError::Sql(err) => write!(f, "database error: {err}"),
        }
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// A note on file, spent or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteRecord {
    pub amount_msat: u64,
    /// When this mint credited the note: where a relative timelock starts.
    pub locked_at: u64,
    pub spent: bool,
    pub pending: bool,
}

/// What an earlier rotate, split, merge or internal transfer of the same set
/// of notes minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Burn {
    pub id: String,
    pub id2: Option<String>,
    pub amount1_msat: u64,
    pub amount2_msat: Option<u64>,
    /// The username an internal transfer credited, which picked `id` itself.
    pub to_username: Option<String>,
}

/// A registered share's note `Q` at an index, as hex, from the `cx1` on file:
/// `None` for an index that derives no key.
pub type DeriveNote<'a> = &'a dyn Fn(&str, u32) -> Option<String>;

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Stats {
    pub outstanding_msat: u64,
    pub outstanding_notes: u64,
    pub spent_notes: u64,
    pub pending_notes: u64,
    pub unpaid_mints: u64,
    pub usernames: u64,
    /// Usernames held for a registration whose address fee is unpaid.
    pub held_usernames: u64,
}

/// A registration held for its address fee (LUD-26).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFee {
    pub payment_hash: String,
    pub pr: String,
    pub username: String,
    pub amount_msat: u64,
    /// What the fee invoice's preimage is keyed by.
    pub label: String,
    pub expires_at: u64,
    pub paid: bool,
}

/// A held registration whose fee was just paid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidAddressFee {
    pub username: String,
    pub amount_msat: u64,
    /// False only if the hold lapsed and somebody else registered the
    /// username before the payment settled: the fee is kept, nothing is
    /// registered.
    pub registered: bool,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonical identity of a burn: the note ids it spent, order independent.
fn burn_key(note_ids: &[String]) -> String {
    let mut sorted: Vec<&str> = note_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join("|")
}

#[derive(Debug)]
pub struct NoteStore {
    conn: Mutex<Connection>,
}

impl NoteStore {
    pub fn open(path: &str) -> StoreResult<Self> {
        let conn = Connection::open(path)?;
        // readers no longer wait on the writer; the journal lives next to the file
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> StoreResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> StoreResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes (
                id TEXT PRIMARY KEY,
                amount_msat INTEGER NOT NULL,
                spent INTEGER NOT NULL DEFAULT 0,
                pending INTEGER NOT NULL DEFAULT 0,
                pending_payment_hash TEXT,
                locked_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS mints (
                payment_hash TEXT PRIMARY KEY,
                pr TEXT NOT NULL,
                amount_msat INTEGER NOT NULL,
                minted INTEGER NOT NULL DEFAULT 0,
                note_id TEXT,
                created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS melts (
                payment_hash TEXT PRIMARY KEY,
                pr TEXT NOT NULL,
                settled INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS burns (
                burn_key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                id2 TEXT,
                amount1_msat INTEGER NOT NULL,
                amount2_msat INTEGER);
             CREATE TABLE IF NOT EXISTS usernames (
                username TEXT PRIMARY KEY,
                cx1 TEXT NOT NULL,
                next_index INTEGER NOT NULL DEFAULT 0,
                nostr_pubkey TEXT);
             CREATE TABLE IF NOT EXISTS operator_invoices (
                payment_hash TEXT PRIMARY KEY,
                amount_msat INTEGER NOT NULL,
                paid INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS address_fees (
                payment_hash TEXT PRIMARY KEY,
                pr TEXT NOT NULL,
                username TEXT NOT NULL,
                cx1 TEXT NOT NULL,
                nostr_pubkey TEXT,
                amount_msat INTEGER NOT NULL,
                label TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                paid INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS address_fees_username ON address_fees (username);",
        )?;
        // columns lnurl-mint renamed and added over time, for a database
        // carried over from it, in lnurl-mint's own order: renames first, so
        // a renamed column is never added a second time under its new name
        for (table, old, new) in [
            ("mints", "comment_hash", "note_id"),
            ("burns", "h", "id"),
            ("burns", "h2", "id2"),
        ] {
            rename_column_if_present(&conn, table, old, new)?;
        }
        for (table, column, ddl) in [
            ("mints", "pr", "TEXT NOT NULL DEFAULT ''"),
            ("notes", "pending", "INTEGER NOT NULL DEFAULT 0"),
            ("notes", "pending_payment_hash", "TEXT"),
            ("notes", "locked_at", "INTEGER NOT NULL DEFAULT 0"),
            ("mints", "note_id", "TEXT"),
            ("mints", "created_at", "INTEGER NOT NULL DEFAULT 0"),
            ("melts", "settled", "INTEGER NOT NULL DEFAULT 0"),
            ("melts", "preimage", "TEXT"),
            ("mints", "zap_request", "TEXT"),
            ("mints", "zap_receipt", "TEXT"),
            ("usernames", "nostr_pubkey", "TEXT"),
            // an address mint picks its note when the payment settles, so its
            // invoice's preimage is keyed by `label` instead (LUD-26)
            ("mints", "username", "TEXT"),
            ("mints", "label", "TEXT"),
            ("burns", "to_username", "TEXT"),
        ] {
            add_column_if_missing(&conn, table, column, ddl)?;
        }
        // Every share key this mint ever accepted, so none is registered
        // twice and a captured proof never applies again (LUD-26). A cx1 on
        // file is hex(x(P) || chain code) from before LUD-26 (64 bytes), or
        // hex(chain code || P) (65 bytes).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS accepted_keys (xp TEXT PRIMARY KEY);
             INSERT OR IGNORE INTO accepted_keys (xp)
                SELECT CASE WHEN length(cx1) = 128 THEN substr(cx1, 1, 64)
                            ELSE substr(cx1, 67, 64) END
                FROM usernames;",
        )?;
        // only once note_id is certain to exist
        conn.execute_batch("CREATE INDEX IF NOT EXISTS mints_note_id ON mints (note_id);")?;
        Ok(NoteStore {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // a panic mid-transaction rolls the transaction back, so the
        // connection itself is still sound
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- minting ----

    /// Record an invoice that credits `note_id` with `amount_msat` once paid.
    /// Refuses a `note_id` already in use, as a note or another mint's output.
    pub fn create_mint(
        &self,
        payment_hash: &str,
        pr: &str,
        amount_msat: u64,
        note_id: &str,
    ) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if id_in_use(&tx, note_id)? {
            return Err(StoreError::InUse);
        }
        tx.execute(
            "INSERT INTO mints (payment_hash, pr, amount_msat, note_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![payment_hash, pr, amount_msat as i64, note_id, now() as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record an invoice that, once paid, credits `amount_msat` to the next
    /// unused note of `username`'s share. The note is picked when the payment
    /// settles, never now, so an unpaid invoice leaves no unused index
    /// (LUD-26). `label` keys the invoice's preimage.
    pub fn create_address_mint(
        &self,
        payment_hash: &str,
        pr: &str,
        amount_msat: u64,
        username: &str,
        label: &str,
    ) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO mints (payment_hash, pr, amount_msat, username, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                payment_hash,
                pr,
                amount_msat as i64,
                username,
                label,
                now() as i64
            ],
        )?;
        Ok(())
    }

    /// Turn a paid mint invoice into an outstanding note. Returns its note id
    /// and value, or `None` if it was already settled (or is unknown). An
    /// address mint picks its note now, with `derive`.
    pub fn settle_mint(
        &self,
        payment_hash: &str,
        derive: DeriveNote<'_>,
    ) -> StoreResult<Option<(String, u64)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE mints SET minted = 1
             WHERE payment_hash = ?1 AND minted = 0
               AND (note_id IS NOT NULL OR username IS NOT NULL)",
            [payment_hash],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let (amount_msat, note_id, username): (i64, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT amount_msat, note_id, username FROM mints WHERE payment_hash = ?1",
                [payment_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let note_id = match (note_id, username) {
            (Some(note_id), _) => note_id,
            (None, Some(username)) => {
                let (note_id, _) = claim_index(&tx, &username, derive)?;
                tx.execute(
                    "UPDATE mints SET note_id = ?1 WHERE payment_hash = ?2",
                    params![note_id, payment_hash],
                )?;
                note_id
            }
            (None, None) => unreachable!("selected for a note id or a username"),
        };
        tx.execute(
            "INSERT INTO notes (id, amount_msat, locked_at) VALUES (?1, ?2, ?3)",
            params![note_id, amount_msat, now() as i64],
        )?;
        tx.commit()?;
        Ok(Some((note_id, amount_msat as u64)))
    }

    /// The payment hash of the unpaid mint invoice that will credit `note_id`.
    pub fn pending_mint_by_note_id(&self, note_id: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT payment_hash FROM mints WHERE note_id = ?1 AND minted = 0",
                [note_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn mint_pr(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT pr FROM mints WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn mint_settled(&self, payment_hash: &str) -> StoreResult<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT minted FROM mints WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            == Some(1))
    }

    // ---- notes ----

    pub fn note_record(&self, note_id: &str) -> StoreResult<Option<NoteRecord>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT amount_msat, locked_at, spent, pending FROM notes WHERE id = ?1",
                [note_id],
                |row| {
                    Ok(NoteRecord {
                        amount_msat: row.get::<_, i64>(0)? as u64,
                        locked_at: row.get::<_, i64>(1)? as u64,
                        spent: row.get::<_, i64>(2)? != 0,
                        pending: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn stats(&self) -> StoreResult<Stats> {
        let conn = self.conn();
        let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0));
        Ok(Stats {
            outstanding_msat: count(
                "SELECT COALESCE(SUM(amount_msat), 0) FROM notes WHERE spent = 0",
            )? as u64,
            outstanding_notes: count("SELECT COUNT(*) FROM notes WHERE spent = 0")? as u64,
            spent_notes: count("SELECT COUNT(*) FROM notes WHERE spent = 1")? as u64,
            pending_notes: count("SELECT COUNT(*) FROM notes WHERE spent = 0 AND pending = 1")?
                as u64,
            unpaid_mints: count("SELECT COUNT(*) FROM mints WHERE minted = 0")? as u64,
            usernames: count("SELECT COUNT(*) FROM usernames")? as u64,
            held_usernames: conn.query_row(
                "SELECT COUNT(*) FROM address_fees WHERE paid = 0 AND expires_at > ?1",
                [now() as i64],
                |row| row.get::<_, i64>(0),
            )? as u64,
        })
    }

    /// Atomically burn every note in `burn_ids` and mint one note per
    /// `(id, amount)` in `outputs`, recording the burn so a retry of the
    /// same mutation can be answered with the same result.
    pub fn swap(&self, burn_ids: &[String], outputs: &[(String, u64)]) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        swap_in(&tx, burn_ids, outputs, None)?;
        tx.commit()?;
        Ok(())
    }

    /// LUD-26's internal transfer: atomically burn `burn_ids` and credit
    /// `amount_msat` to the next unused note of `username`'s share. Returns
    /// the note credited.
    pub fn transfer_to_username(
        &self,
        burn_ids: &[String],
        username: &str,
        amount_msat: u64,
        derive: DeriveNote<'_>,
    ) -> StoreResult<String> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let (note_id, _) = claim_index(&tx, username, derive)?;
        swap_in(
            &tx,
            burn_ids,
            &[(note_id.clone(), amount_msat)],
            Some(username),
        )?;
        tx.commit()?;
        Ok(note_id)
    }

    /// The outputs of an earlier rotate/split/merge of exactly this set of notes.
    pub fn find_burn(&self, note_ids: &[String]) -> StoreResult<Option<Burn>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, id2, amount1_msat, amount2_msat, to_username FROM burns
                 WHERE burn_key = ?1",
                [burn_key(note_ids)],
                |row| {
                    Ok(Burn {
                        id: row.get(0)?,
                        id2: row.get(1)?,
                        amount1_msat: row.get::<_, i64>(2)? as u64,
                        amount2_msat: row.get::<_, Option<i64>>(3)?.map(|a| a as u64),
                        to_username: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    // ---- melts ----

    /// Reserve notes for an in-flight melt without burning them. All or
    /// nothing; every other request naming one is refused as "pending".
    pub fn mark_pending(&self, note_ids: &[String], payment_hash: &str) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in note_ids {
            let pending: Option<i64> = tx
                .query_row(
                    "SELECT pending FROM notes WHERE id = ?1 AND spent = 0",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            match pending {
                None => return Err(StoreError::Invalid),
                Some(p) if p != 0 => return Err(StoreError::Pending),
                _ => {}
            }
        }
        for id in note_ids {
            tx.execute(
                "UPDATE notes SET pending = 1, pending_payment_hash = ?1 WHERE id = ?2",
                params![payment_hash, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Burn the notes the melt paying `payment_hash` reserved, once its
    /// payment is confirmed settled, and keep its preimage for LUD-21. Returns
    /// the notes burned and their value; nothing on a repeat, though a
    /// preimage learned late is still kept.
    pub fn finalize_melt(
        &self,
        payment_hash: &str,
        preimage: Option<&str>,
    ) -> StoreResult<(Vec<String>, u64)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut burned = vec![];
        let mut total = 0u64;
        {
            let mut stmt = tx.prepare(
                "SELECT id, amount_msat FROM notes
                 WHERE pending = 1 AND spent = 0 AND pending_payment_hash = ?1",
            )?;
            for row in stmt.query_map([payment_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })? {
                let (id, amount) = row?;
                burned.push(id);
                total += amount as u64;
            }
        }
        tx.execute(
            "UPDATE notes SET spent = 1, pending = 0, pending_payment_hash = NULL
             WHERE pending = 1 AND spent = 0 AND pending_payment_hash = ?1",
            [payment_hash],
        )?;
        tx.execute(
            "UPDATE melts SET settled = 1, preimage = COALESCE(?2, preimage)
             WHERE payment_hash = ?1",
            params![payment_hash, preimage],
        )?;
        tx.commit()?;
        Ok((burned, total))
    }

    /// Release the notes the melt paying `payment_hash` reserved, once its
    /// payment is confirmed not to have gone out. Never after the melt
    /// settled: LDK can report a failure after a success, and the success
    /// stands. Returns the notes released.
    pub fn restore_melt(&self, payment_hash: &str) -> StoreResult<Vec<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let restored = restore_melt(&tx, payment_hash)?;
        tx.commit()?;
        Ok(restored)
    }

    /// A melt that provably never left: release its notes and forget it, so
    /// its invoice may be melted into again.
    pub fn abort_melt(&self, payment_hash: &str) -> StoreResult<Vec<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let restored = restore_melt(&tx, payment_hash)?;
        tx.execute(
            "DELETE FROM melts WHERE payment_hash = ?1 AND settled = 0",
            [payment_hash],
        )?;
        tx.commit()?;
        Ok(restored)
    }

    pub fn melt_preimage(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT preimage FROM melts WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    // ---- payments the node receives ----

    /// The note an incoming payment to `payment_hash` may be claimed for: it
    /// pays an unsettled mint invoice. Anything else is failed back, a second
    /// payment of a settled invoice included.
    pub fn claimable(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        let conn = self.conn();
        let mint = conn
            .query_row(
                "SELECT COALESCE(label, note_id) FROM mints
                 WHERE payment_hash = ?1 AND minted = 0
                   AND (note_id IS NOT NULL OR username IS NOT NULL)",
                [payment_hash],
                |row| row.get(0),
            )
            .optional()?;
        if mint.is_some() {
            return Ok(mint);
        }
        // an address fee, while its hold stands: once it lapsed the username
        // may be somebody else's, so a late payment is failed back
        Ok(conn
            .query_row(
                "SELECT label FROM address_fees
                 WHERE payment_hash = ?1 AND paid = 0 AND expires_at > ?2",
                params![payment_hash, now() as i64],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// What a mint invoice's preimage is keyed by: the note it credits, or
    /// for an address mint its label.
    pub fn mint_preimage_label(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT COALESCE(label, note_id) FROM mints WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Every note reserved by a melt, grouped by that melt's payment hash.
    pub fn pending_melts(&self) -> StoreResult<BTreeMap<String, Vec<String>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, pending_payment_hash FROM notes
             WHERE pending = 1 AND spent = 0 AND pending_payment_hash IS NOT NULL",
        )?;
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))? {
            let (id, hash) = row?;
            grouped.entry(hash).or_default().push(id);
        }
        Ok(grouped)
    }

    pub fn record_melt(&self, payment_hash: &str, pr: &str) -> StoreResult<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO melts (payment_hash, pr) VALUES (?1, ?2)",
            params![payment_hash, pr],
        )?;
        Ok(())
    }

    pub fn melt_pr(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT pr FROM melts WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn melt_settled(&self, payment_hash: &str) -> StoreResult<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT settled FROM melts WHERE payment_hash = ?1",
                [payment_hash],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            == Some(1))
    }

    // ---- usernames (LUD-26) ----

    /// Register `username` for a share. Never overwrites: a registered
    /// username is `AlreadyRegistered`, and a share key this mint accepted
    /// before is `KeyUsed`. The caller has already checked the proof.
    pub fn register_username(
        &self,
        username: &str,
        cx1_hex: &str,
        xp_hex: &str,
        nostr_pubkey_hex: Option<&str>,
    ) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if username_taken(&tx, username)? {
            return Err(StoreError::AlreadyRegistered);
        }
        accept_key(&tx, xp_hex)?;
        tx.execute(
            "INSERT INTO usernames (username, cx1, nostr_pubkey, next_index) VALUES (?1, ?2, ?3, 0)",
            params![username, cx1_hex, nostr_pubkey_hex],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether `username` is registered, or held for an unpaid address fee.
    pub fn username_taken(&self, username: &str) -> StoreResult<bool> {
        Ok(username_taken(&self.conn(), username)?)
    }

    /// Hold `username` for a registration until its address fee invoice
    /// `payment_hash` is paid, or released once it expires at `expires_at`
    /// (LUD-26). Refuses what `register_username` refuses, and takes the
    /// share key now, so a captured proof never applies again even if the
    /// invoice lapses. The caller has already checked the proof.
    #[allow(clippy::too_many_arguments)]
    pub fn hold_username(
        &self,
        payment_hash: &str,
        pr: &str,
        username: &str,
        cx1_hex: &str,
        xp_hex: &str,
        nostr_pubkey_hex: Option<&str>,
        amount_msat: u64,
        label: &str,
        expires_at: u64,
    ) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if username_taken(&tx, username)? {
            return Err(StoreError::AlreadyRegistered);
        }
        accept_key(&tx, xp_hex)?;
        tx.execute(
            "INSERT INTO address_fees (payment_hash, pr, username, cx1, nostr_pubkey,
                amount_msat, label, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                payment_hash,
                pr,
                username,
                cx1_hex,
                nostr_pubkey_hex,
                amount_msat as i64,
                label,
                now() as i64,
                expires_at as i64
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The address fee invoice `payment_hash`, paid or not.
    pub fn address_fee(&self, payment_hash: &str) -> StoreResult<Option<AddressFee>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT payment_hash, pr, username, amount_msat, label, expires_at, paid
                 FROM address_fees WHERE payment_hash = ?1",
                [payment_hash],
                address_fee_row,
            )
            .optional()?)
    }

    /// Every username held for an unpaid, unexpired address fee.
    pub fn held_usernames(&self) -> StoreResult<Vec<AddressFee>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT payment_hash, pr, username, amount_msat, label, expires_at, paid
             FROM address_fees WHERE paid = 0 AND expires_at > ?1 ORDER BY username",
        )?;
        let rows = stmt.query_map([now() as i64], address_fee_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Register the username a paid address fee held. `None` if the invoice
    /// was settled before (or is unknown). Never checks the expiry: the
    /// payment was claimed while the hold stood.
    pub fn settle_address_fee(&self, payment_hash: &str) -> StoreResult<Option<PaidAddressFee>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let paid = settle_address_fee_in(&tx, payment_hash)?;
        tx.commit()?;
        Ok(paid)
    }

    /// Pay the address fee invoice `payment_hash` with notes this mint
    /// issued: burn `note_ids`, worth `total_msat`, and register the held
    /// username, all at once. The invoice must be unpaid, unexpired and for
    /// exactly `total_msat`. Returns the username registered.
    pub fn pay_address_fee_with_notes(
        &self,
        note_ids: &[String],
        payment_hash: &str,
        total_msat: u64,
    ) -> StoreResult<String> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let payable: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM address_fees WHERE payment_hash = ?1
                AND paid = 0 AND expires_at > ?2 AND amount_msat = ?3)",
            params![payment_hash, now() as i64, total_msat as i64],
            |row| row.get(0),
        )?;
        if !payable {
            return Err(StoreError::FeeNotPayable);
        }
        burn_in(&tx, note_ids)?;
        let paid = settle_address_fee_in(&tx, payment_hash)?.ok_or(StoreError::FeeNotPayable)?;
        if !paid.registered {
            // the hold stood a moment ago: never burn for nothing
            return Err(StoreError::AlreadyRegistered);
        }
        tx.commit()?;
        Ok(paid.username)
    }

    /// Move a registered `username` to a new share without ever freeing it
    /// (LUD-26's PUT). Auto-mint starts over at the new share's index 0.
    pub fn update_username(&self, username: &str, cx1_hex: &str, xp_hex: &str) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        accept_key(&tx, xp_hex)?;
        let changed = tx.execute(
            "UPDATE usernames SET cx1 = ?1, next_index = 0 WHERE username = ?2",
            params![cx1_hex, username],
        )?;
        if changed != 1 {
            return Err(StoreError::UnknownUser);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_username(&self, username: &str) -> StoreResult<()> {
        self.conn()
            .execute("DELETE FROM usernames WHERE username = ?1", [username])?;
        Ok(())
    }

    /// `hex(P || chain_code)` registered under `username`.
    pub fn username_branch(&self, username: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT cx1 FROM usernames WHERE username = ?1",
                [username],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Every registered username with its `hex(P || chain_code)` and next index.
    pub fn list_usernames(&self) -> StoreResult<Vec<(String, String, u32)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT username, cx1, next_index FROM usernames ORDER BY username")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u32))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn nostr_pubkey(&self, username: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT nostr_pubkey FROM usernames WHERE username = ?1",
                [username],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }
}

/// Burn `burn_ids` and mint `outputs` inside `tx`, recording the burn so a
/// retry is answered with the same result. `to_username` marks an internal
/// transfer, whose output the mint picked.
fn swap_in(
    tx: &rusqlite::Transaction<'_>,
    burn_ids: &[String],
    outputs: &[(String, u64)],
    to_username: Option<&str>,
) -> StoreResult<()> {
    burn_in(tx, burn_ids)?;
    let mut seen = HashSet::new();
    for (id, _) in outputs {
        if !seen.insert(id) || id_in_use(tx, id)? {
            return Err(StoreError::InUse);
        }
    }
    let locked_at = now() as i64;
    for (id, amount) in outputs {
        tx.execute(
            "INSERT INTO notes (id, amount_msat, locked_at) VALUES (?1, ?2, ?3)",
            params![id, *amount as i64, locked_at],
        )?;
    }
    tx.execute(
        "INSERT INTO burns (burn_key, id, id2, amount1_msat, amount2_msat, to_username)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            burn_key(burn_ids),
            outputs[0].0,
            outputs.get(1).map(|o| o.0.clone()),
            outputs[0].1 as i64,
            outputs.get(1).map(|o| o.1 as i64),
            to_username,
        ],
    )?;
    Ok(())
}

/// Burn `note_ids` inside `tx`: every one unspent and not reserved by a melt.
fn burn_in(tx: &rusqlite::Transaction<'_>, note_ids: &[String]) -> StoreResult<()> {
    let mut seen = HashSet::new();
    for id in note_ids {
        if !seen.insert(id) {
            return Err(StoreError::Invalid);
        }
        let pending: Option<i64> = tx
            .query_row(
                "SELECT pending FROM notes WHERE id = ?1 AND spent = 0",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        match pending {
            None => return Err(StoreError::Invalid),
            Some(p) if p != 0 => return Err(StoreError::Pending),
            _ => {}
        }
    }
    for id in note_ids {
        tx.execute("UPDATE notes SET spent = 1 WHERE id = ?1", [id])?;
    }
    Ok(())
}

/// Whether `username` is registered, or held for an address fee whose
/// invoice is unpaid and unexpired.
fn username_taken(conn: &Connection, username: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM usernames WHERE username = ?1)
             OR EXISTS(SELECT 1 FROM address_fees
                       WHERE username = ?1 AND paid = 0 AND expires_at > ?2)",
        params![username, now() as i64],
        |row| row.get(0),
    )
}

/// Mark the address fee `payment_hash` paid and register its username,
/// unless somebody else holds it by now.
fn settle_address_fee_in(
    tx: &rusqlite::Transaction<'_>,
    payment_hash: &str,
) -> StoreResult<Option<PaidAddressFee>> {
    let changed = tx.execute(
        "UPDATE address_fees SET paid = 1 WHERE payment_hash = ?1 AND paid = 0",
        [payment_hash],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    let (username, cx1_hex, nostr_pubkey, amount_msat): (String, String, Option<String>, i64) = tx
        .query_row(
            "SELECT username, cx1, nostr_pubkey, amount_msat FROM address_fees
             WHERE payment_hash = ?1",
            [payment_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    // its own hold is paid now, so only a registration (or another hold,
    // once this one lapsed) can stand in the way
    let registered = !username_taken(tx, &username)?;
    if registered {
        tx.execute(
            "INSERT INTO usernames (username, cx1, nostr_pubkey, next_index) VALUES (?1, ?2, ?3, 0)",
            params![username, cx1_hex, nostr_pubkey],
        )?;
    }
    Ok(Some(PaidAddressFee {
        username,
        amount_msat: amount_msat as u64,
        registered,
    }))
}

fn address_fee_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AddressFee> {
    Ok(AddressFee {
        payment_hash: row.get(0)?,
        pr: row.get(1)?,
        username: row.get(2)?,
        amount_msat: row.get::<_, i64>(3)? as u64,
        label: row.get(4)?,
        expires_at: row.get::<_, i64>(5)? as u64,
        paid: row.get::<_, i64>(6)? != 0,
    })
}

/// Record a share key as accepted, refusing one accepted before.
fn accept_key(tx: &rusqlite::Transaction<'_>, xp_hex: &str) -> StoreResult<()> {
    let fresh = tx.execute(
        "INSERT OR IGNORE INTO accepted_keys (xp) VALUES (?1)",
        [xp_hex],
    )?;
    if fresh != 1 {
        return Err(StoreError::KeyUsed);
    }
    Ok(())
}

/// Claim (and persist past) the next unused index on `username`'s share,
/// skipping any whose note is outstanding, burned or another mint's output.
fn claim_index(
    tx: &rusqlite::Transaction<'_>,
    username: &str,
    derive: DeriveNote<'_>,
) -> StoreResult<(String, u32)> {
    let Some((cx1_hex, start)): Option<(String, i64)> = tx
        .query_row(
            "SELECT cx1, next_index FROM usernames WHERE username = ?1",
            [username],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
    else {
        return Err(StoreError::UnknownUser);
    };
    let mut index = start as u32;
    let note_id = loop {
        if let Some(id) = derive(&cx1_hex, index) {
            if !id_in_use(tx, &id)? {
                break id;
            }
        }
        index = index.checked_add(1).ok_or(StoreError::InUse)?;
    };
    tx.execute(
        "UPDATE usernames SET next_index = ?1 WHERE username = ?2",
        params![i64::from(index) + 1, username],
    )?;
    Ok((note_id, index))
}

/// Whether `note_id` names a note (spent or not) or some mint's output.
fn id_in_use(conn: &Connection, note_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM notes WHERE id = ?1)
             OR EXISTS(SELECT 1 FROM mints WHERE note_id = ?1)",
        [note_id],
        |row| row.get(0),
    )
}

fn restore_melt(
    tx: &rusqlite::Transaction<'_>,
    payment_hash: &str,
) -> rusqlite::Result<Vec<String>> {
    let settled: Option<i64> = tx
        .query_row(
            "SELECT settled FROM melts WHERE payment_hash = ?1",
            [payment_hash],
            |row| row.get(0),
        )
        .optional()?;
    if settled == Some(1) {
        return Ok(vec![]);
    }
    let restored = {
        let mut stmt = tx.prepare(
            "SELECT id FROM notes WHERE pending = 1 AND spent = 0 AND pending_payment_hash = ?1",
        )?;
        stmt.query_map([payment_hash], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute(
        "UPDATE notes SET pending = 0, pending_payment_hash = NULL
         WHERE pending = 1 AND spent = 0 AND pending_payment_hash = ?1",
        [payment_hash],
    )?;
    Ok(restored)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rename_column_if_present(
    conn: &Connection,
    table: &str,
    old: &str,
    new: &str,
) -> rusqlite::Result<()> {
    if has_column(conn, table, old)? && !has_column(conn, table, new)? {
        conn.execute(
            &format!("ALTER TABLE {table} RENAME COLUMN {old} TO {new}"),
            [],
        )?;
    }
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> rusqlite::Result<()> {
    if !has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A settlement with no registered share to credit.
    fn no_share(_: &str, _: u32) -> Option<String> {
        None
    }

    fn minted(store: &NoteStore, hash: &str, id: &str, amount: u64) {
        store.create_mint(hash, "lnbc", amount, id).unwrap();
        assert_eq!(
            store.settle_mint(hash, &no_share).unwrap(),
            Some((id.to_string(), amount))
        );
    }

    /// lnurl-mint databases as they were before its later migrations: the
    /// oldest (no pending, no pr) and one from before the renames.
    #[test]
    fn older_lnurl_mint_databases_open() {
        let oldest = Connection::open_in_memory().unwrap();
        oldest
            .execute_batch(
                "CREATE TABLE notes (id TEXT PRIMARY KEY, amount_msat INTEGER NOT NULL,
                    spent INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE mints (payment_hash TEXT PRIMARY KEY,
                    amount_msat INTEGER NOT NULL, minted INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO notes (id, amount_msat) VALUES ('aa', 5000);",
            )
            .unwrap();
        let store = NoteStore::init(oldest).unwrap();
        assert_eq!(store.note_record("aa").unwrap().unwrap().amount_msat, 5000);
        store.create_mint("h1", "lnbc", 7000, "bb").unwrap();
        assert_eq!(
            store.settle_mint("h1", &no_share).unwrap(),
            Some(("bb".into(), 7000))
        );

        let renamed = Connection::open_in_memory().unwrap();
        renamed
            .execute_batch(
                "CREATE TABLE notes (id TEXT PRIMARY KEY, amount_msat INTEGER NOT NULL,
                    spent INTEGER NOT NULL DEFAULT 0, pending INTEGER NOT NULL DEFAULT 0,
                    pending_payment_hash TEXT);
                 CREATE TABLE mints (payment_hash TEXT PRIMARY KEY, pr TEXT NOT NULL,
                    amount_msat INTEGER NOT NULL, minted INTEGER NOT NULL DEFAULT 0,
                    comment_hash TEXT, zap_request TEXT, zap_receipt TEXT,
                    created_at INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE burns (burn_key TEXT PRIMARY KEY, h TEXT NOT NULL, h2 TEXT,
                    amount1_msat INTEGER NOT NULL, amount2_msat INTEGER);
                 INSERT INTO mints (payment_hash, pr, amount_msat, comment_hash)
                    VALUES ('h2', 'lnbc', 9000, 'cc');
                 INSERT INTO burns (burn_key, h, amount1_msat) VALUES ('dd', 'ee', 4000);",
            )
            .unwrap();
        let store = NoteStore::init(renamed).unwrap();
        // an unpaid invoice from before still credits its note once paid
        assert_eq!(
            store.pending_mint_by_note_id("cc").unwrap().as_deref(),
            Some("h2")
        );
        assert_eq!(
            store.settle_mint("h2", &no_share).unwrap(),
            Some(("cc".into(), 9000))
        );
        // and a burn from before still answers its retry
        let burn = store.find_burn(&ids(&["dd"])).unwrap().unwrap();
        assert_eq!((burn.id.as_str(), burn.amount1_msat), ("ee", 4000));
    }

    #[test]
    fn mint_settles_once() {
        let store = NoteStore::in_memory().unwrap();
        minted(&store, "h1", "q1", 5000);
        assert_eq!(store.settle_mint("h1", &no_share).unwrap(), None);
        let record = store.note_record("q1").unwrap().unwrap();
        assert_eq!(record.amount_msat, 5000);
        assert!(!record.spent);
        assert!(matches!(
            store.create_mint("h2", "lnbc", 1, "q1"),
            Err(StoreError::InUse)
        ));
    }

    #[test]
    fn swap_is_atomic_and_recorded() {
        let store = NoteStore::in_memory().unwrap();
        minted(&store, "h1", "q1", 5000);
        minted(&store, "h2", "q2", 3000);
        // an unknown note fails the whole swap
        assert!(matches!(
            store.swap(&ids(&["q1", "nope"]), &[("q3".into(), 8000)]),
            Err(StoreError::Invalid)
        ));
        assert!(!store.note_record("q1").unwrap().unwrap().spent);
        // an output already in use fails it too
        assert!(matches!(
            store.swap(&ids(&["q1"]), &[("q2".into(), 5000)]),
            Err(StoreError::InUse)
        ));
        store
            .swap(&ids(&["q2", "q1"]), &[("q3".into(), 8000)])
            .unwrap();
        assert!(store.note_record("q1").unwrap().unwrap().spent);
        let burn = store.find_burn(&ids(&["q1", "q2"])).unwrap().unwrap();
        assert_eq!(burn.id, "q3");
        assert_eq!(burn.amount1_msat, 8000);
        // a burned id can never be minted again
        assert!(matches!(
            store.create_mint("h9", "lnbc", 1, "q1"),
            Err(StoreError::InUse)
        ));
    }

    #[test]
    fn pending_notes_refuse_everything_until_resolved() {
        let store = NoteStore::in_memory().unwrap();
        minted(&store, "h1", "q1", 5000);
        store.record_melt("m1", "lnbc").unwrap();
        store.mark_pending(&ids(&["q1"]), "m1").unwrap();
        assert!(matches!(
            store.swap(&ids(&["q1"]), &[("q2".into(), 5000)]),
            Err(StoreError::Pending)
        ));
        assert!(matches!(
            store.mark_pending(&ids(&["q1"]), "m2"),
            Err(StoreError::Pending)
        ));
        assert_eq!(store.pending_melts().unwrap()["m1"], ids(&["q1"]));
        assert_eq!(store.restore_melt("m1").unwrap(), ids(&["q1"]));
        store.mark_pending(&ids(&["q1"]), "m1").unwrap();
        assert_eq!(
            store.finalize_melt("m1", Some("pre")).unwrap(),
            (ids(&["q1"]), 5000)
        );
        assert!(store.note_record("q1").unwrap().unwrap().spent);
        assert!(store.melt_settled("m1").unwrap());
        assert_eq!(store.melt_preimage("m1").unwrap().as_deref(), Some("pre"));
        // a repeat burns nothing more, and keeps the preimage
        assert_eq!(store.finalize_melt("m1", None).unwrap(), (vec![], 0));
        assert_eq!(store.melt_preimage("m1").unwrap().as_deref(), Some("pre"));
        // a failure reported after the success changes nothing
        assert!(store.restore_melt("m1").unwrap().is_empty());
        assert!(store.note_record("q1").unwrap().unwrap().spent);
    }

    #[test]
    fn an_aborted_melt_frees_its_notes_and_its_invoice() {
        let store = NoteStore::in_memory().unwrap();
        minted(&store, "h1", "q1", 5000);
        store.record_melt("m1", "lnbc").unwrap();
        store.mark_pending(&ids(&["q1"]), "m1").unwrap();
        assert_eq!(store.abort_melt("m1").unwrap(), ids(&["q1"]));
        assert!(!store.note_record("q1").unwrap().unwrap().pending);
        assert_eq!(store.melt_pr("m1").unwrap(), None);
    }

    #[test]
    fn only_unpaid_mint_invoices_are_claimable() {
        let store = NoteStore::in_memory().unwrap();
        store.create_mint("h1", "lnbc", 5000, "q1").unwrap();
        assert_eq!(store.claimable("h1").unwrap().as_deref(), Some("q1"));
        assert_eq!(store.claimable("nobody").unwrap(), None);
        store.settle_mint("h1", &no_share).unwrap();
        // a second payment of a paid invoice is failed back
        assert_eq!(store.claimable("h1").unwrap(), None);
        assert_eq!(
            store.mint_preimage_label("h1").unwrap().as_deref(),
            Some("q1")
        );
    }

    /// The share note `k<i>` at index `i`.
    fn share_note(_: &str, index: u32) -> Option<String> {
        Some(format!("k{index}"))
    }

    #[test]
    fn an_address_mint_picks_its_note_when_it_settles() {
        let store = NoteStore::in_memory().unwrap();
        store.register_username("alice", "00", "x0", None).unwrap();
        // two invoices, neither paid: no index is taken yet
        store
            .create_address_mint("h1", "lnbc", 1000, "alice", "a1")
            .unwrap();
        store
            .create_address_mint("h2", "lnbc", 2000, "alice", "a2")
            .unwrap();
        assert_eq!(store.claimable("h1").unwrap().as_deref(), Some("a1"));
        assert_eq!(
            store.mint_preimage_label("h2").unwrap().as_deref(),
            Some("a2")
        );
        // k0 is somebody's already, so the first to settle skips it
        minted(&store, "h0", "k0", 1);
        assert_eq!(
            store.settle_mint("h2", &share_note).unwrap(),
            Some(("k1".into(), 2000))
        );
        assert_eq!(
            store.settle_mint("h1", &share_note).unwrap(),
            Some(("k2".into(), 1000))
        );
        assert_eq!(store.settle_mint("h1", &share_note).unwrap(), None);
        assert_eq!(store.claimable("h1").unwrap(), None);
    }

    #[test]
    fn a_transfer_burns_and_credits_the_next_share_note_at_once() {
        let store = NoteStore::in_memory().unwrap();
        store.register_username("alice", "00", "x0", None).unwrap();
        minted(&store, "h1", "q1", 5000);
        let credited = store
            .transfer_to_username(&ids(&["q1"]), "alice", 5000, &share_note)
            .unwrap();
        assert_eq!(credited, "k0");
        assert!(store.note_record("q1").unwrap().unwrap().spent);
        assert_eq!(store.note_record("k0").unwrap().unwrap().amount_msat, 5000);
        let burn = store.find_burn(&ids(&["q1"])).unwrap().unwrap();
        assert_eq!(burn.to_username.as_deref(), Some("alice"));
        // a spent note transfers nothing, and takes no index
        assert!(matches!(
            store.transfer_to_username(&ids(&["q1"]), "alice", 5000, &share_note),
            Err(StoreError::Invalid)
        ));
        minted(&store, "h2", "q2", 1);
        assert_eq!(
            store
                .transfer_to_username(&ids(&["q2"]), "alice", 1, &share_note)
                .unwrap(),
            "k1"
        );
        assert!(matches!(
            store.transfer_to_username(&ids(&["q2"]), "bob", 1, &share_note),
            Err(StoreError::UnknownUser)
        ));
    }

    #[test]
    fn a_username_is_never_overwritten_and_a_share_never_reused() {
        let store = NoteStore::in_memory().unwrap();
        store.register_username("alice", "00", "x0", None).unwrap();
        assert!(matches!(
            store.register_username("alice", "01", "x1", None),
            Err(StoreError::AlreadyRegistered)
        ));
        assert!(matches!(
            store.register_username("bob", "00", "x0", None),
            Err(StoreError::KeyUsed)
        ));
        store.update_username("alice", "01", "x1").unwrap();
        assert_eq!(
            store.username_branch("alice").unwrap().as_deref(),
            Some("01")
        );
        assert!(matches!(
            store.update_username("alice", "00", "x0"),
            Err(StoreError::KeyUsed)
        ));
        assert!(matches!(
            store.update_username("bob", "02", "x2"),
            Err(StoreError::UnknownUser)
        ));
        store.delete_username("alice").unwrap();
        assert!(matches!(
            store.register_username("alice", "01", "x1", None),
            Err(StoreError::KeyUsed)
        ));
    }

    /// Hold `username` for share `xp` behind fee invoice `hash`, expiring
    /// `ttl` seconds from now (negative: lapsed already).
    fn hold(store: &NoteStore, hash: &str, username: &str, xp: &str, ttl: i64) {
        store
            .hold_username(
                hash,
                "lnbc",
                username,
                &format!("cx1-{xp}"),
                xp,
                None,
                21_000,
                &format!("label-{hash}"),
                (now() as i64 + ttl) as u64,
            )
            .unwrap();
    }

    #[test]
    fn a_held_username_registers_once_its_fee_is_paid() {
        let store = NoteStore::in_memory().unwrap();
        hold(&store, "f1", "alice", "x0", 3600);
        assert!(store.username_taken("alice").unwrap());
        assert_eq!(store.username_branch("alice").unwrap(), None);
        assert_eq!(store.stats().unwrap().held_usernames, 1);
        assert_eq!(store.held_usernames().unwrap()[0].username, "alice");
        // held for this registration only: nobody else gets it meanwhile
        assert!(matches!(
            store.register_username("alice", "01", "x1", None),
            Err(StoreError::AlreadyRegistered)
        ));
        assert!(matches!(
            store.hold_username("f2", "lnbc", "alice", "01", "x1", None, 1, "l", now() + 60),
            Err(StoreError::AlreadyRegistered)
        ));
        // and its share is taken at once
        assert!(matches!(
            store.register_username("bob", "00", "x0", None),
            Err(StoreError::KeyUsed)
        ));
        assert_eq!(store.claimable("f1").unwrap().as_deref(), Some("label-f1"));

        let paid = store.settle_address_fee("f1").unwrap().unwrap();
        assert_eq!(
            paid,
            PaidAddressFee {
                username: "alice".into(),
                amount_msat: 21_000,
                registered: true
            }
        );
        assert_eq!(
            store.username_branch("alice").unwrap().as_deref(),
            Some("cx1-x0")
        );
        assert_eq!(store.settle_address_fee("f1").unwrap(), None);
        assert_eq!(store.claimable("f1").unwrap(), None);
        assert!(store.address_fee("f1").unwrap().unwrap().paid);
        let stats = store.stats().unwrap();
        assert_eq!((stats.usernames, stats.held_usernames), (1, 0));
    }

    #[test]
    fn a_lapsed_hold_frees_the_username_but_never_its_share() {
        let store = NoteStore::in_memory().unwrap();
        hold(&store, "f1", "alice", "x0", -1);
        assert!(!store.username_taken("alice").unwrap());
        // a late payment is failed back
        assert_eq!(store.claimable("f1").unwrap(), None);
        assert!(matches!(
            store.register_username("alice", "00", "x0", None),
            Err(StoreError::KeyUsed)
        ));
        store.register_username("alice", "01", "x1", None).unwrap();
        // one claimed just before it lapsed still settles, and registers
        // nothing over the new owner
        assert_eq!(
            store
                .settle_address_fee("f1")
                .unwrap()
                .map(|p| p.registered),
            Some(false)
        );
        assert_eq!(
            store.username_branch("alice").unwrap().as_deref(),
            Some("01")
        );
    }

    #[test]
    fn notes_pay_an_address_fee_exactly_and_atomically() {
        let store = NoteStore::in_memory().unwrap();
        hold(&store, "f1", "alice", "x0", 3600);
        minted(&store, "h1", "q1", 20_000);
        minted(&store, "h2", "q2", 21_000);
        // not the fee: nothing burned, nothing registered
        assert!(matches!(
            store.pay_address_fee_with_notes(&ids(&["q1"]), "f1", 20_000),
            Err(StoreError::FeeNotPayable)
        ));
        // a note mid-melt pays nothing
        store.record_melt("m1", "lnbc").unwrap();
        store.mark_pending(&ids(&["q2"]), "m1").unwrap();
        assert!(matches!(
            store.pay_address_fee_with_notes(&ids(&["q2"]), "f1", 21_000),
            Err(StoreError::Pending)
        ));
        store.restore_melt("m1").unwrap();
        assert_eq!(
            store
                .pay_address_fee_with_notes(&ids(&["q2"]), "f1", 21_000)
                .unwrap(),
            "alice"
        );
        assert!(store.note_record("q2").unwrap().unwrap().spent);
        assert!(!store.note_record("q1").unwrap().unwrap().spent);
        assert!(store.username_branch("alice").unwrap().is_some());
        // paid once
        minted(&store, "h3", "q3", 21_000);
        assert!(matches!(
            store.pay_address_fee_with_notes(&ids(&["q3"]), "f1", 21_000),
            Err(StoreError::FeeNotPayable)
        ));
        assert!(!store.note_record("q3").unwrap().unwrap().spent);
        // and never into a lapsed hold
        hold(&store, "f2", "bob", "x5", -1);
        assert!(matches!(
            store.pay_address_fee_with_notes(&ids(&["q3"]), "f2", 21_000),
            Err(StoreError::FeeNotPayable)
        ));
    }

    /// A share registered before the table of accepted keys existed is in
    /// it once the store opens: both cx1 shapes on file.
    #[test]
    fn shares_on_file_are_accepted_keys() {
        let conn = Connection::open_in_memory().unwrap();
        let legacy = format!("{}{}", "aa".repeat(32), "cc".repeat(32));
        let current = format!("{}02{}", "cc".repeat(32), "bb".repeat(32));
        conn.execute_batch(&format!(
            "CREATE TABLE usernames (username TEXT PRIMARY KEY, cx1 TEXT NOT NULL,
                next_index INTEGER NOT NULL DEFAULT 0);
             INSERT INTO usernames (username, cx1) VALUES ('old', '{legacy}'), ('new', '{current}');"
        ))
        .unwrap();
        let store = NoteStore::init(conn).unwrap();
        for xp in ["aa".repeat(32), "bb".repeat(32)] {
            assert!(matches!(
                store.register_username("carol", "ff", &xp, None),
                Err(StoreError::KeyUsed)
            ));
        }
    }
}
