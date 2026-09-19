//! Encrypted SQLite ledger. On disk: MYPH1 blob. While unlocked: `.workspace.sqlite`.

use std::fs;
use std::path::{Path, PathBuf};

use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::ai::{AiSecret, AiSettings, DEFAULT_THRESHOLD};
use crate::crypto::{self, DataKey};
use crate::error::Error;
use crate::providers::{AccountSet, ConnectionSecrets, NormalizedTxn};

const LEDGER_FILE: &str = "ledger.enc";

/// Seconds a status toast stays up unless the user changes it in Setup → Log.
pub const DEFAULT_STATUS_TIMEOUT_SECS: u64 = 5;
pub const MIN_STATUS_TIMEOUT_SECS: u64 = 1;
pub const MAX_STATUS_TIMEOUT_SECS: u64 = 120;

pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
    key: DataKey,
    salt: [u8; 16],
}

impl Store {
    /// Create a new ledger in `dir`, or unlock an existing one.
    pub fn open(dir: &Path, passphrase: &str) -> Result<Self, Error> {
        fs::create_dir_all(dir)?;
        let path = dir.join(LEDGER_FILE);
        if path.exists() {
            Self::unlock(dir, passphrase)
        } else {
            Self::create(dir, passphrase)
        }
    }

    fn workspace(dir: &Path) -> PathBuf {
        dir.join(".workspace.sqlite")
    }

    fn create(dir: &Path, passphrase: &str) -> Result<Self, Error> {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let key = crypto::derive_key(passphrase, &salt)?;
        let wp = Self::workspace(dir);
        let _ = fs::remove_file(&wp);
        let conn = Connection::open(&wp)?;
        migrate(&conn)?;
        let store = Self {
            conn,
            data_dir: dir.to_path_buf(),
            key,
            salt,
        };
        store.persist()?;
        Ok(store)
    }

    fn unlock(dir: &Path, passphrase: &str) -> Result<Self, Error> {
        let blob = fs::read(dir.join(LEDGER_FILE))?;
        let dec = crypto::decrypt(passphrase, &blob)?;
        let wp = Self::workspace(dir);
        fs::write(&wp, &dec.plaintext)?;
        let conn = Connection::open(&wp)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            data_dir: dir.to_path_buf(),
            key: dec.key.clone(),
            salt: dec.salt,
        })
    }

    pub fn persist(&self) -> Result<(), Error> {
        let dump = self
            .data_dir
            .join(format!(".dump-{}.sqlite", Uuid::new_v4()));
        self.conn
            .execute("VACUUM INTO ?1", params![dump.to_str().unwrap()])?;
        let plaintext = fs::read(&dump)?;
        let _ = fs::remove_file(&dump);
        let enc = crypto::encrypt_existing(&self.key, &self.salt, &plaintext)?;
        let tmp_enc = self.data_dir.join(format!(".enc-{}.tmp", Uuid::new_v4()));
        fs::write(&tmp_enc, &enc)?;
        fs::rename(&tmp_enc, self.data_dir.join(LEDGER_FILE))?;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn add_connection(
        &self,
        source_id: &str,
        name: &str,
        secrets: &ConnectionSecrets,
    ) -> Result<String, Error> {
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO connections (id, source_id, name, secrets, created_at) VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![id, source_id, name, secrets.inner],
        )?;
        self.persist()?;
        Ok(id)
    }

    pub fn connection_secrets(&self, connection_id: &str) -> Result<ConnectionSecrets, Error> {
        let inner: String = self.conn.query_row(
            "SELECT secrets FROM connections WHERE id = ?1",
            params![connection_id],
            |r| r.get(0),
        )?;
        Ok(ConnectionSecrets { inner })
    }

    /// AI settings, with defaults for anything not saved yet. The key sits in the ledger like
    /// SimpleFIN Access URLs do: encrypted at rest, never logged.
    pub fn ai_settings(&self) -> Result<AiSettings, Error> {
        let get = |key: &str| -> Result<Option<String>, Error> {
            Ok(self
                .conn
                .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                    r.get(0)
                })
                .optional()?)
        };
        let provider = get("ai_provider")?.filter(|p| !p.is_empty());
        let api_key = AiSecret(get("ai_api_key")?.unwrap_or_default());
        let threshold = get("ai_threshold")?
            .and_then(|v| v.parse::<f64>().ok())
            .map(|t| t.clamp(0.0, 1.0))
            .unwrap_or(DEFAULT_THRESHOLD);
        let after_sync = get("ai_after_sync")?.as_deref() == Some("1");
        Ok(AiSettings {
            provider,
            api_key,
            threshold,
            after_sync,
        })
    }

    pub fn set_ai_settings(&self, settings: &AiSettings) -> Result<(), Error> {
        let pairs = [
            ("ai_provider", settings.provider.clone().unwrap_or_default()),
            ("ai_api_key", settings.api_key.0.trim().to_string()),
            (
                "ai_threshold",
                format!("{:.4}", settings.threshold.clamp(0.0, 1.0)),
            ),
            (
                "ai_after_sync",
                if settings.after_sync { "1" } else { "0" }.to_string(),
            ),
        ];
        for (k, v) in pairs {
            self.conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![k, v],
            )?;
        }
        self.persist()
    }

    /// Whether debug views are on: raw bank data and AI traces on each transaction.
    pub fn debug_enabled(&self) -> Result<bool, Error> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key='debug'", [], |r| r.get(0))
            .optional()?;
        Ok(v.as_deref() == Some("1"))
    }

    /// Turn the debug views on or off.
    pub fn set_debug_enabled(&self, on: bool) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('debug', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![if on { "1" } else { "0" }],
        )?;
        self.persist()
    }

    /// How long a status toast stays up, in seconds. Falls back to the default when nothing
    /// is saved or the saved value does not parse.
    pub fn status_timeout_secs(&self) -> Result<u64, Error> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='status_timeout_secs'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok())
            .filter(|s| (MIN_STATUS_TIMEOUT_SECS..=MAX_STATUS_TIMEOUT_SECS).contains(s))
            .unwrap_or(DEFAULT_STATUS_TIMEOUT_SECS))
    }

    /// Save the toast timeout. Rejects anything outside 1..=120 seconds.
    pub fn set_status_timeout_secs(&self, secs: u64) -> Result<(), Error> {
        if !(MIN_STATUS_TIMEOUT_SECS..=MAX_STATUS_TIMEOUT_SECS).contains(&secs) {
            return Err(Error::user(format!(
                "Status timeout must be {MIN_STATUS_TIMEOUT_SECS} to {MAX_STATUS_TIMEOUT_SECS} seconds."
            )));
        }
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('status_timeout_secs', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![secs.to_string()],
        )?;
        self.persist()
    }

    /// The bank's JSON for one transaction and its account, pretty-printed, or `None` for a
    /// row that is gone. A split line has no raw copy of its own, so it shows its parent's.
    /// Either part is `None` when the row was imported before raw data was kept.
    pub fn raw_transaction(&self, txn_id: &str) -> Result<Option<RawTxnData>, Error> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT COALESCE(t.raw_json, p.raw_json), a.raw_json
                 FROM transactions t
                 JOIN accounts a ON a.id = t.account_id
                 LEFT JOIN transactions p ON p.id = t.parent_id
                 WHERE t.id=?1",
                params![txn_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(txn, account)| RawTxnData {
            transaction: txn.map(|j| pretty_json(&j)),
            account: account.map(|j| pretty_json(&j)),
        }))
    }

    pub fn list_connections(&self) -> Result<Vec<ConnectionRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, name, last_sync_at, last_error FROM connections ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ConnectionRow {
                id: r.get(0)?,
                source_id: r.get(1)?,
                name: r.get(2)?,
                last_sync_at: r.get(3)?,
                last_error: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Remove a connection with its accounts and transactions. Tombstones for it go too,
    /// so re-adding the same bank later re-imports everything.
    pub fn delete_connection(&self, connection_id: &str) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM transactions WHERE account_id IN (SELECT id FROM accounts WHERE connection_id=?1)",
            params![connection_id],
        )?;
        tx.execute(
            "DELETE FROM accounts WHERE connection_id=?1",
            params![connection_id],
        )?;
        tx.execute(
            "DELETE FROM tombstones WHERE connection_id=?1",
            params![connection_id],
        )?;
        let n = tx.execute(
            "DELETE FROM connections WHERE id=?1",
            params![connection_id],
        )?;
        tx.commit()?;
        if n == 0 {
            return Err(Error::user("That connection is gone."));
        }
        self.persist()?;
        Ok(())
    }

    pub fn set_sync_result(&self, connection_id: &str, error: Option<&str>) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE connections SET last_sync_at = unixepoch(), last_error = ?2 WHERE id = ?1",
            params![connection_id, error],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn upsert_imported(
        &self,
        connection_id: &str,
        set: &AccountSet,
    ) -> Result<ImportStats, Error> {
        let mut stats = ImportStats::default();
        for acc in &set.accounts {
            let account_id = self.upsert_account(connection_id, acc)?;
            for txn in &acc.transactions {
                match self.import_txn(connection_id, &account_id, &acc.remote_id, txn)? {
                    ImportAction::Inserted => stats.inserted += 1,
                    ImportAction::Updated => stats.updated += 1,
                    ImportAction::SkippedTombstone => stats.skipped_tombstone += 1,
                    ImportAction::MatchedPending => stats.matched_pending += 1,
                }
            }
        }
        self.persist()?;
        Ok(stats)
    }

    fn upsert_account(
        &self,
        connection_id: &str,
        acc: &crate::providers::NormalizedAccount,
    ) -> Result<String, Error> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM accounts WHERE connection_id = ?1 AND remote_id = ?2",
                params![connection_id, acc.remote_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            self.conn.execute(
                "UPDATE accounts SET name=?2, currency=?3, balance_cents=?4, available_cents=?5, balance_date=?6, raw_json=?7, institution=?8 WHERE id=?1",
                params![id, acc.name, acc.currency, acc.balance_cents, acc.available_cents, acc.balance_date, acc.raw, acc.institution],
            )?;
            Ok(id)
        } else {
            let id = Uuid::new_v4().to_string();
            self.conn.execute(
                "INSERT INTO accounts (id, connection_id, remote_id, name, currency, balance_cents, available_cents, balance_date, raw_json, institution)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    id,
                    connection_id,
                    acc.remote_id,
                    acc.name,
                    acc.currency,
                    acc.balance_cents,
                    acc.available_cents,
                    acc.balance_date,
                    acc.raw,
                    acc.institution
                ],
            )?;
            Ok(id)
        }
    }

    fn import_txn(
        &self,
        connection_id: &str,
        account_id: &str,
        remote_account_id: &str,
        txn: &NormalizedTxn,
    ) -> Result<ImportAction, Error> {
        let tomb: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM tombstones WHERE connection_id=?1 AND remote_account_id=?2 AND remote_txn_id=?3",
                params![connection_id, remote_account_id, txn.remote_id],
                |r| r.get(0),
            )
            .optional()?;
        if tomb.is_some() {
            return Ok(ImportAction::SkippedTombstone);
        }

        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM transactions WHERE account_id=?1 AND remote_id=?2 AND parent_id IS NULL",
                params![account_id, txn.remote_id],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            // Keep user overrides; only refresh bank originals.
            self.conn.execute(
                "UPDATE transactions SET posted_at=?2, transacted_at=?3, amount_cents=?4, description=?5, pending=?6, raw_json=?7
                 WHERE id=?1",
                params![
                    id,
                    txn.posted,
                    txn.transacted_at,
                    txn.amount_cents,
                    txn.description,
                    txn.pending as i64,
                    txn.raw
                ],
            )?;
            return Ok(ImportAction::Updated);
        }

        if !txn.pending {
            if let Some(pending_id) = self.find_pending_match(account_id, txn)? {
                self.conn.execute(
                    "UPDATE transactions SET remote_id=?2, posted_at=?3, transacted_at=?4, amount_cents=?5, description=?6, pending=0, raw_json=?7 WHERE id=?1",
                    params![
                        pending_id,
                        txn.remote_id,
                        txn.posted,
                        txn.transacted_at,
                        txn.amount_cents,
                        txn.description,
                        txn.raw
                    ],
                )?;
                return Ok(ImportAction::MatchedPending);
            }
        }

        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO transactions (
                id, account_id, remote_id, posted_at, transacted_at, amount_cents, description, pending, raw_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                account_id,
                txn.remote_id,
                txn.posted,
                txn.transacted_at,
                txn.amount_cents,
                txn.description,
                txn.pending as i64,
                txn.raw
            ],
        )?;
        Ok(ImportAction::Inserted)
    }

    fn find_pending_match(
        &self,
        account_id: &str,
        txn: &NormalizedTxn,
    ) -> Result<Option<String>, Error> {
        let normalized = crate::domain::normalize_payee(&txn.description);
        let mut stmt = self.conn.prepare(
            "SELECT id, description, posted_at FROM transactions
             WHERE account_id=?1 AND pending=1 AND deleted=0 AND parent_id IS NULL AND amount_cents=?2",
        )?;
        let mut rows = stmt.query(params![account_id, txn.amount_cents])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let desc: String = row.get(1)?;
            let posted: i64 = row.get(2)?;
            if crate::domain::normalize_payee(&desc) == normalized {
                let window = 14 * 86400;
                if (posted - txn.posted).abs() <= window || posted == 0 || txn.posted == 0 {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    pub fn tombstone_and_delete(&self, txn_id: &str) -> Result<(), Error> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT t.remote_id, a.remote_id, a.connection_id
                 FROM transactions t JOIN accounts a ON a.id = t.account_id
                 WHERE t.id = ?1",
                params![txn_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((remote_txn, remote_acct, conn_id)) = row {
            self.conn.execute(
                "INSERT OR IGNORE INTO tombstones (connection_id, remote_account_id, remote_txn_id) VALUES (?1,?2,?3)",
                params![conn_id, remote_acct, remote_txn],
            )?;
        }
        self.conn.execute(
            "UPDATE transactions SET deleted=1 WHERE id=?1 OR parent_id=?1",
            params![txn_id],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.persist();
        let _ = fs::remove_file(Self::workspace(&self.data_dir));
    }
}

fn migrate(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS connections (
            id TEXT PRIMARY KEY,
            source_id TEXT NOT NULL,
            name TEXT NOT NULL,
            secrets TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_sync_at INTEGER,
            last_error TEXT
        );
        CREATE TABLE IF NOT EXISTS accounts (
            id TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL REFERENCES connections(id),
            remote_id TEXT NOT NULL,
            name TEXT NOT NULL,
            currency TEXT NOT NULL,
            balance_cents INTEGER NOT NULL,
            available_cents INTEGER,
            balance_date INTEGER,
            hidden INTEGER NOT NULL DEFAULT 0,
            UNIQUE(connection_id, remote_id)
        );
        CREATE TABLE IF NOT EXISTS categories (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS transactions (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id),
            remote_id TEXT NOT NULL,
            posted_at INTEGER NOT NULL,
            transacted_at INTEGER,
            amount_cents INTEGER NOT NULL,
            description TEXT NOT NULL,
            pending INTEGER NOT NULL DEFAULT 0,
            user_payee TEXT,
            user_posted_at INTEGER,
            user_amount_cents INTEGER,
            notes TEXT,
            category_id TEXT REFERENCES categories(id),
            excluded INTEGER NOT NULL DEFAULT 0,
            deleted INTEGER NOT NULL DEFAULT 0,
            transfer_peer_id TEXT,
            transfer_account_id TEXT,
            parent_id TEXT,
            categorized_by TEXT,
            is_income INTEGER NOT NULL DEFAULT 0,
            UNIQUE(account_id, remote_id)
        );
        CREATE TABLE IF NOT EXISTS tombstones (
            connection_id TEXT NOT NULL,
            remote_account_id TEXT NOT NULL,
            remote_txn_id TEXT NOT NULL,
            PRIMARY KEY (connection_id, remote_account_id, remote_txn_id)
        );
        CREATE TABLE IF NOT EXISTS budgets (
            category_id TEXT NOT NULL REFERENCES categories(id),
            year INTEGER NOT NULL,
            month INTEGER NOT NULL,
            cap_cents INTEGER NOT NULL,
            PRIMARY KEY (category_id, year, month)
        );
        CREATE TABLE IF NOT EXISTS rules (
            id TEXT PRIMARY KEY,
            priority INTEGER NOT NULL,
            description_pattern TEXT,
            description_regex TEXT,
            amount_min_cents INTEGER,
            amount_max_cents INTEGER,
            account_id TEXT,
            category_id TEXT REFERENCES categories(id),
            enabled INTEGER NOT NULL DEFAULT 1,
            action TEXT NOT NULL DEFAULT 'category'
        );
        CREATE TABLE IF NOT EXISTS payee_memory (
            normalized_payee TEXT PRIMARY KEY,
            category_id TEXT NOT NULL REFERENCES categories(id)
        );
        INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '3');
        "#,
    )?;
    migrate_v2(conn)?;
    migrate_v3(conn)?;
    migrate_v4(conn)?;
    migrate_v5(conn)?;
    migrate_v6(conn)?;
    migrate_v7(conn)?;
    migrate_v8(conn)?;
    migrate_v9(conn)?;
    Ok(())
}

/// True when `table` already has a column named `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names.iter().any(|n| n == column))
}

/// Schema 1 → 2: hidden accounts, income flag, and rule actions (rules.category_id becomes
/// nullable, which SQLite can only do by rebuilding the table). Each step is idempotent.
fn migrate_v2(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "accounts", "hidden")? {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_column(conn, "transactions", "is_income")? {
        conn.execute_batch(
            "ALTER TABLE transactions ADD COLUMN is_income INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !has_column(conn, "rules", "action")? {
        conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE rules_v2 (
                id TEXT PRIMARY KEY,
                priority INTEGER NOT NULL,
                description_contains TEXT,
                description_regex TEXT,
                amount_min_cents INTEGER,
                amount_max_cents INTEGER,
                account_id TEXT,
                category_id TEXT REFERENCES categories(id),
                enabled INTEGER NOT NULL DEFAULT 1,
                action TEXT NOT NULL DEFAULT 'category'
            );
            INSERT INTO rules_v2 (id, priority, description_contains, description_regex, amount_min_cents, amount_max_cents, account_id, category_id, enabled)
              SELECT id, priority, description_contains, description_regex, amount_min_cents, amount_max_cents, account_id, category_id, enabled FROM rules;
            DROP TABLE rules;
            ALTER TABLE rules_v2 RENAME TO rules;
            COMMIT;
            "#,
        )?;
    }
    conn.execute("UPDATE meta SET value='2' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 2 → 3: rule phrases become wildcard patterns matched against the whole description.
/// The column is renamed and every existing "contains" phrase is wrapped in `*` so it keeps
/// matching the same rows. Idempotent: skipped once the column has its new name.
fn migrate_v3(conn: &Connection) -> Result<(), Error> {
    if has_column(conn, "rules", "description_contains")? {
        conn.execute_batch(
            r#"
            BEGIN;
            ALTER TABLE rules RENAME COLUMN description_contains TO description_pattern;
            UPDATE rules SET description_pattern = '*' || description_pattern || '*'
              WHERE description_pattern IS NOT NULL AND description_pattern != '';
            COMMIT;
            "#,
        )?;
    }
    conn.execute("UPDATE meta SET value='3' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 3 → 4: transfers can carry a kind (`card` or `loan` payment) so credit card and loan
/// payments are labelled as such while still being ignored by the budget. Idempotent.
fn migrate_v4(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "transactions", "transfer_kind")? {
        conn.execute_batch("ALTER TABLE transactions ADD COLUMN transfer_kind TEXT;")?;
    }
    conn.execute("UPDATE meta SET value='4' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 4 → 5: categories carry a description (context for AI categorization) and
/// `ai_answers` caches one AI answer per normalized payee so a payee is asked once. Idempotent.
fn migrate_v5(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "categories", "description")? {
        conn.execute_batch("ALTER TABLE categories ADD COLUMN description TEXT;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ai_answers (
            normalized_payee TEXT PRIMARY KEY,
            category_id TEXT,
            confidence REAL NOT NULL,
            provider TEXT NOT NULL,
            asked_at INTEGER NOT NULL
        );",
    )?;
    conn.execute("UPDATE meta SET value='5' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 5 → 6: accounts and transactions keep the source's JSON object verbatim
/// (`raw_json`, filled on the next sync), and `ai_answers` keeps what was sent to the provider
/// and what came back (`exchanges`, a JSON array of `AiExchange`). Idempotent.
fn migrate_v6(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "accounts", "raw_json")? {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN raw_json TEXT;")?;
    }
    if !has_column(conn, "transactions", "raw_json")? {
        conn.execute_batch("ALTER TABLE transactions ADD COLUMN raw_json TEXT;")?;
    }
    if !has_column(conn, "ai_answers", "exchanges")? {
        conn.execute_batch("ALTER TABLE ai_answers ADD COLUMN exchanges TEXT;")?;
    }
    conn.execute("UPDATE meta SET value='6' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 6 → 7: a failed AI call is remembered. `transactions.ai_failed_at` marks rows whose
/// last AI attempt errored (skipped by the after-sync pass), and `ai_answers.error` keeps the
/// sanitized error with its exchanges so the trace can show what went wrong. Idempotent.
fn migrate_v7(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "transactions", "ai_failed_at")? {
        conn.execute_batch("ALTER TABLE transactions ADD COLUMN ai_failed_at INTEGER;")?;
    }
    if !has_column(conn, "ai_answers", "error")? {
        conn.execute_batch("ALTER TABLE ai_answers ADD COLUMN error TEXT;")?;
    }
    conn.execute("UPDATE meta SET value='7' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 7 → 8: a category may sit under one top-level parent (`parent_id`) and may be kept
/// out of the budget (`in_budget = 0`: no cap, not on Month, not in the month's spent).
/// Existing rows stay top-level and in budget. Idempotent.
fn migrate_v8(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "categories", "parent_id")? {
        conn.execute_batch(
            "ALTER TABLE categories ADD COLUMN parent_id TEXT REFERENCES categories(id);",
        )?;
    }
    if !has_column(conn, "categories", "in_budget")? {
        conn.execute_batch(
            "ALTER TABLE categories ADD COLUMN in_budget INTEGER NOT NULL DEFAULT 1;",
        )?;
    }
    conn.execute("UPDATE meta SET value='8' WHERE key='schema'", [])?;
    Ok(())
}

/// Schema 8 → 9: `accounts.institution` names the bank an account lives at. It comes from
/// the source's connection list, which is not kept in `raw_json`, so rows fill on the next
/// sync. Idempotent.
fn migrate_v9(conn: &Connection) -> Result<(), Error> {
    if !has_column(conn, "accounts", "institution")? {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN institution TEXT;")?;
    }
    conn.execute("UPDATE meta SET value='9' WHERE key='schema'", [])?;
    Ok(())
}

/// Pretty-print stored JSON; anything that is not JSON comes back as it was.
fn pretty_json(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}

/// What the bank sent for one transaction, as stored on import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTxnData {
    pub transaction: Option<String>,
    pub account: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub inserted: u32,
    pub updated: u32,
    pub skipped_tombstone: u32,
    pub matched_pending: u32,
}

enum ImportAction {
    Inserted,
    Updated,
    SkippedTombstone,
    MatchedPending,
}

#[derive(Debug, Clone)]
pub struct ConnectionRow {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::NormalizedAccount;

    /// A ledger created before rule actions, hidden accounts, and the income flag existed.
    fn v1_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE connections (id TEXT PRIMARY KEY, source_id TEXT NOT NULL, name TEXT NOT NULL, secrets TEXT NOT NULL, created_at INTEGER NOT NULL, last_sync_at INTEGER, last_error TEXT);
            CREATE TABLE accounts (id TEXT PRIMARY KEY, connection_id TEXT NOT NULL REFERENCES connections(id), remote_id TEXT NOT NULL, name TEXT NOT NULL, currency TEXT NOT NULL, balance_cents INTEGER NOT NULL, available_cents INTEGER, balance_date INTEGER, UNIQUE(connection_id, remote_id));
            CREATE TABLE categories (id TEXT PRIMARY KEY, name TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE transactions (id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), remote_id TEXT NOT NULL, posted_at INTEGER NOT NULL, transacted_at INTEGER, amount_cents INTEGER NOT NULL, description TEXT NOT NULL, pending INTEGER NOT NULL DEFAULT 0, user_payee TEXT, user_posted_at INTEGER, user_amount_cents INTEGER, notes TEXT, category_id TEXT REFERENCES categories(id), excluded INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0, transfer_peer_id TEXT, transfer_account_id TEXT, parent_id TEXT, categorized_by TEXT, UNIQUE(account_id, remote_id));
            CREATE TABLE rules (id TEXT PRIMARY KEY, priority INTEGER NOT NULL, description_contains TEXT, description_regex TEXT, amount_min_cents INTEGER, amount_max_cents INTEGER, account_id TEXT, category_id TEXT NOT NULL REFERENCES categories(id), enabled INTEGER NOT NULL DEFAULT 1);
            INSERT INTO meta (key, value) VALUES ('schema', '1');
            INSERT INTO categories (id, name) VALUES ('c1', 'Food');
            INSERT INTO rules (id, priority, description_contains, category_id) VALUES ('r1', 1, 'costco', 'c1');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn migrate_v1_ledger_keeps_rules_and_adds_columns() {
        let conn = Connection::open_in_memory().unwrap();
        v1_schema(&conn);
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent

        assert!(has_column(&conn, "accounts", "hidden").unwrap());
        assert!(has_column(&conn, "transactions", "is_income").unwrap());
        assert!(has_column(&conn, "transactions", "transfer_kind").unwrap());
        assert!(has_column(&conn, "categories", "description").unwrap());
        conn.execute(
            "INSERT INTO ai_answers (normalized_payee, category_id, confidence, provider, asked_at)
             VALUES ('costco', NULL, 0.3, 'typesafe', 1)",
            [],
        )
        .unwrap();
        assert!(has_column(&conn, "rules", "action").unwrap());
        assert!(has_column(&conn, "rules", "description_pattern").unwrap());
        assert!(!has_column(&conn, "rules", "description_contains").unwrap());
        let (cat, action, pattern): (Option<String>, String, String) = conn
            .query_row(
                "SELECT category_id, action, description_pattern FROM rules WHERE id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(cat.as_deref(), Some("c1"));
        assert_eq!(action, "category");
        // The old "contains costco" phrase still matches the same rows as a pattern.
        assert_eq!(pattern, "*costco*");
        // category_id is nullable now, so flag rules can be stored.
        conn.execute(
            "INSERT INTO rules (id, priority, description_pattern, action) VALUES ('r2', 2, 'payroll', 'income')",
            [],
        )
        .unwrap();
        let schema: String = conn
            .query_row("SELECT value FROM meta WHERE key='schema'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(has_column(&conn, "accounts", "raw_json").unwrap());
        assert!(has_column(&conn, "transactions", "raw_json").unwrap());
        assert!(has_column(&conn, "ai_answers", "exchanges").unwrap());
        assert!(has_column(&conn, "transactions", "ai_failed_at").unwrap());
        assert!(has_column(&conn, "ai_answers", "error").unwrap());
        assert!(has_column(&conn, "categories", "parent_id").unwrap());
        assert!(has_column(&conn, "categories", "in_budget").unwrap());
        // Old categories stay top-level and in budget; a child can point at one of them.
        let (parent, in_budget): (Option<String>, i64) = conn
            .query_row(
                "SELECT parent_id, in_budget FROM categories WHERE id='c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(parent, None);
        assert_eq!(in_budget, 1);
        conn.execute(
            "INSERT INTO categories (id, name, parent_id, in_budget) VALUES ('c2', 'Fees', 'c1', 0)",
            [],
        )
        .unwrap();
        assert!(has_column(&conn, "accounts", "institution").unwrap());
        assert_eq!(schema, "9");
    }

    fn one_txn_set(remote_id: &str, raw: Option<&str>, acc_raw: Option<&str>) -> AccountSet {
        AccountSet {
            errors: vec![],
            accounts: vec![NormalizedAccount {
                remote_id: "acct".into(),
                conn_id: "c".into(),
                name: "Checking".into(),
                institution: Some("Example Bank".into()),
                currency: "USD".into(),
                balance_cents: 0,
                available_cents: None,
                balance_date: 1_700_000_000,
                transactions: vec![NormalizedTxn {
                    remote_id: remote_id.into(),
                    posted: 1_700_000_100,
                    transacted_at: None,
                    amount_cents: -500,
                    description: "Coffee".into(),
                    pending: false,
                    raw: raw.map(String::from),
                }],
                raw: acc_raw.map(String::from),
            }],
        }
    }

    #[test]
    fn raw_json_is_stored_refreshed_and_pretty_printed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "simplefin",
                "Demo",
                &ConnectionSecrets { inner: "x".into() },
            )
            .unwrap();

        // Imported before raw data was kept: both parts are missing.
        store
            .upsert_imported(&cid, &one_txn_set("t1", None, None))
            .unwrap();
        let id = store.list_transactions(false, None).unwrap()[0].id.clone();
        let raw = store.raw_transaction(&id).unwrap().unwrap();
        assert_eq!(
            raw,
            RawTxnData {
                transaction: None,
                account: None
            }
        );

        // The next sync fills them in and re-syncs replace them.
        store
            .upsert_imported(
                &cid,
                &one_txn_set(
                    "t1",
                    Some(r#"{"id":"t1","extra":{"a":1}}"#),
                    Some(r#"{"id":"acct"}"#),
                ),
            )
            .unwrap();
        let raw = store.raw_transaction(&id).unwrap().unwrap();
        assert_eq!(
            raw.transaction.as_deref(),
            Some("{\n  \"extra\": {\n    \"a\": 1\n  },\n  \"id\": \"t1\"\n}")
        );
        assert_eq!(raw.account.as_deref(), Some("{\n  \"id\": \"acct\"\n}"));

        // A split line shows its parent's raw copy.
        let cat = store.add_category("Food").unwrap();
        store
            .split_transaction(&id, &[(Some(cat.clone()), -200), (Some(cat), -300)])
            .unwrap();
        let child: String = store
            .conn()
            .query_row(
                "SELECT id FROM transactions WHERE parent_id=?1 LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        let child_raw = store.raw_transaction(&child).unwrap().unwrap();
        assert_eq!(child_raw.transaction, raw.transaction);

        assert!(store.raw_transaction("nope").unwrap().is_none());
    }

    #[test]
    fn debug_flag_defaults_off_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        assert!(!store.debug_enabled().unwrap());
        store.set_debug_enabled(true).unwrap();
        assert!(store.debug_enabled().unwrap());
        store.set_debug_enabled(false).unwrap();
        assert!(!store.debug_enabled().unwrap());
    }

    #[test]
    fn status_timeout_defaults_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        assert_eq!(
            store.status_timeout_secs().unwrap(),
            DEFAULT_STATUS_TIMEOUT_SECS
        );
        store.set_status_timeout_secs(12).unwrap();
        assert_eq!(store.status_timeout_secs().unwrap(), 12);
        assert!(store.set_status_timeout_secs(0).is_err());
        assert!(store.set_status_timeout_secs(121).is_err());
        assert_eq!(store.status_timeout_secs().unwrap(), 12);
        // Garbage in meta falls back to the default instead of failing.
        store
            .conn
            .execute(
                "UPDATE meta SET value='abc' WHERE key='status_timeout_secs'",
                [],
            )
            .unwrap();
        assert_eq!(
            store.status_timeout_secs().unwrap(),
            DEFAULT_STATUS_TIMEOUT_SECS
        );
    }

    #[test]
    fn ai_settings_roundtrip_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let s = store.ai_settings().unwrap();
        assert!(s.provider.is_none());
        assert!(s.api_key.is_empty());
        assert!((s.threshold - 0.70).abs() < 1e-9);
        assert!(!s.after_sync);
        assert!(!s.is_configured());

        store
            .set_ai_settings(&AiSettings {
                provider: Some("typesafe".into()),
                api_key: AiSecret(" sk-1 ".into()),
                threshold: 1.5,
                after_sync: true,
            })
            .unwrap();
        let s = store.ai_settings().unwrap();
        assert_eq!(s.provider.as_deref(), Some("typesafe"));
        assert_eq!(s.api_key.0, "sk-1");
        assert!((s.threshold - 1.0).abs() < 1e-9);
        assert!(s.after_sync);
        assert!(s.is_configured());

        // Clearing the provider turns the feature off again.
        store
            .set_ai_settings(&AiSettings {
                provider: None,
                ..s
            })
            .unwrap();
        assert!(!store.ai_settings().unwrap().is_configured());
    }
}
