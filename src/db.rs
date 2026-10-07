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

/// What an earlier rotate, split or merge of the same set of notes minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Burn {
    pub id: String,
    pub id2: Option<String>,
    pub amount1_msat: u64,
    pub amount2_msat: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Stats {
    pub outstanding_msat: u64,
    pub outstanding_notes: u64,
    pub spent_notes: u64,
    pub pending_notes: u64,
    pub unpaid_mints: u64,
    pub usernames: u64,
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
                created_at INTEGER NOT NULL DEFAULT 0);",
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
        ] {
            add_column_if_missing(&conn, table, column, ddl)?;
        }
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

    /// Turn a paid mint invoice into an outstanding note. Returns its note id
    /// and value, or `None` if it was already settled (or is unknown).
    pub fn settle_mint(&self, payment_hash: &str) -> StoreResult<Option<(String, u64)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE mints SET minted = 1
             WHERE payment_hash = ?1 AND minted = 0 AND note_id IS NOT NULL",
            [payment_hash],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let (amount_msat, note_id): (i64, String) = tx.query_row(
            "SELECT amount_msat, note_id FROM mints WHERE payment_hash = ?1",
            [payment_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
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
        })
    }

    /// Atomically burn every note in `burn_ids` and mint one note per
    /// `(id, amount)` in `outputs`, recording the burn so a retry of the
    /// same mutation can be answered with the same result.
    pub fn swap(&self, burn_ids: &[String], outputs: &[(String, u64)]) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut seen = HashSet::new();
        for id in burn_ids {
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
        let mut seen = HashSet::new();
        for (id, _) in outputs {
            if !seen.insert(id) || id_in_use(&tx, id)? {
                return Err(StoreError::InUse);
            }
        }
        for id in burn_ids {
            tx.execute("UPDATE notes SET spent = 1 WHERE id = ?1", [id])?;
        }
        let locked_at = now() as i64;
        for (id, amount) in outputs {
            tx.execute(
                "INSERT INTO notes (id, amount_msat, locked_at) VALUES (?1, ?2, ?3)",
                params![id, *amount as i64, locked_at],
            )?;
        }
        tx.execute(
            "INSERT INTO burns (burn_key, id, id2, amount1_msat, amount2_msat)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                burn_key(burn_ids),
                outputs[0].0,
                outputs.get(1).map(|o| o.0.clone()),
                outputs[0].1 as i64,
                outputs.get(1).map(|o| o.1 as i64),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The outputs of an earlier rotate/split/merge of exactly this set of notes.
    pub fn find_burn(&self, note_ids: &[String]) -> StoreResult<Option<Burn>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, id2, amount1_msat, amount2_msat FROM burns WHERE burn_key = ?1",
                [burn_key(note_ids)],
                |row| {
                    Ok(Burn {
                        id: row.get(0)?,
                        id2: row.get(1)?,
                        amount1_msat: row.get::<_, i64>(2)? as u64,
                        amount2_msat: row.get::<_, Option<i64>>(3)?.map(|a| a as u64),
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
        Ok(self
            .conn()
            .query_row(
                "SELECT note_id FROM mints
                 WHERE payment_hash = ?1 AND minted = 0 AND note_id IS NOT NULL",
                [payment_hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The note a mint invoice credits, settled or not.
    pub fn mint_note_id(&self, payment_hash: &str) -> StoreResult<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT note_id FROM mints WHERE payment_hash = ?1",
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

    /// Claim `username` for a branch, or replace an existing claim wholesale.
    /// The caller has already checked the ownership proof.
    pub fn upsert_username(
        &self,
        username: &str,
        cx1_hex: &str,
        nostr_pubkey_hex: Option<&str>,
    ) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO usernames (username, cx1, nostr_pubkey, next_index) VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(username) DO UPDATE SET
                cx1 = excluded.cx1, nostr_pubkey = excluded.nostr_pubkey, next_index = 0",
            params![username, cx1_hex, nostr_pubkey_hex],
        )?;
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

    pub fn next_index_hint(&self, username: &str) -> StoreResult<Option<u32>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT next_index FROM usernames WHERE username = ?1",
                [username],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|i| i as u32))
    }

    /// Reserve (and persist past) the next free index on `username`'s branch.
    pub fn claim_next_index(
        &self,
        username: &str,
        derive: impl Fn(u32) -> Option<String>,
    ) -> StoreResult<(String, u32)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(start): Option<i64> = tx
            .query_row(
                "SELECT next_index FROM usernames WHERE username = ?1",
                [username],
                |row| row.get(0),
            )
            .optional()?
        else {
            return Err(StoreError::Invalid);
        };
        let mut index = start as u32;
        let note_id = loop {
            if let Some(id) = derive(index) {
                if !id_in_use(&tx, &id)? {
                    break id;
                }
            }
            index = index.checked_add(1).ok_or(StoreError::InUse)?;
        };
        tx.execute(
            "UPDATE usernames SET next_index = ?1 WHERE username = ?2",
            params![i64::from(index) + 1, username],
        )?;
        tx.commit()?;
        Ok((note_id, index))
    }
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

    fn minted(store: &NoteStore, hash: &str, id: &str, amount: u64) {
        store.create_mint(hash, "lnbc", amount, id).unwrap();
        assert_eq!(
            store.settle_mint(hash).unwrap(),
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
        assert_eq!(store.settle_mint("h1").unwrap(), Some(("bb".into(), 7000)));

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
        assert_eq!(store.settle_mint("h2").unwrap(), Some(("cc".into(), 9000)));
        // and a burn from before still answers its retry
        let burn = store.find_burn(&ids(&["dd"])).unwrap().unwrap();
        assert_eq!((burn.id.as_str(), burn.amount1_msat), ("ee", 4000));
    }

    #[test]
    fn mint_settles_once() {
        let store = NoteStore::in_memory().unwrap();
        minted(&store, "h1", "q1", 5000);
        assert_eq!(store.settle_mint("h1").unwrap(), None);
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
        store.settle_mint("h1").unwrap();
        // a second payment of a paid invoice is failed back
        assert_eq!(store.claimable("h1").unwrap(), None);
        assert_eq!(store.mint_note_id("h1").unwrap().as_deref(), Some("q1"));
    }

    #[test]
    fn address_indices_skip_keys_in_use() {
        let store = NoteStore::in_memory().unwrap();
        store.upsert_username("alice", "00", None).unwrap();
        minted(&store, "h1", "k0", 1000);
        let (id, index) = store
            .claim_next_index("alice", |i| Some(format!("k{i}")))
            .unwrap();
        assert_eq!((id.as_str(), index), ("k1", 1));
        assert_eq!(store.next_index_hint("alice").unwrap(), Some(2));
        let (id, _) = store
            .claim_next_index("alice", |i| Some(format!("k{i}")))
            .unwrap();
        store.create_mint("h2", "lnbc", 7, &id).unwrap();
        assert!(store.pending_mint_by_note_id("k2").unwrap().is_some());
    }
}
