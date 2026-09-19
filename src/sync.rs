//! Pull windows from a TransactionSource and import without duplicating rows.

use crate::error::Error;
use crate::providers::{
    chunk_windows, DateWindow, TransactionSource, Transport, MAX_WINDOW_DAYS, OVERLAP_DAYS,
};
use crate::sanitize::sanitize_user_text;
use crate::store::{ImportStats, Store};

pub struct SyncReport {
    pub stats: ImportStats,
    pub errors: Vec<String>,
}

/// How far back first connect / resync pulls. Split into [`MAX_WINDOW_DAYS`] chunks.
pub const DEFAULT_HISTORY_DAYS: i64 = 90;

/// Sync one connection. Windows are ≤45 days with 5-day overlap.
pub fn sync_connection(
    store: &Store,
    connection_id: &str,
    source: &dyn TransactionSource,
    transport: &dyn Transport,
    start: i64,
    end: i64,
) -> Result<SyncReport, Error> {
    let secrets = store.connection_secrets(connection_id)?;
    let mut stats = ImportStats::default();
    let mut errors = Vec::new();
    let windows = chunk_windows(start, end, MAX_WINDOW_DAYS, OVERLAP_DAYS);
    if windows.is_empty() {
        windows_fallback(start, end);
    }
    for window in windows {
        match source.fetch(&secrets, window, true, transport) {
            Ok(set) => {
                for err in &set.errors {
                    errors.push(sanitize_user_text(&format!(
                        "{}: {}",
                        err.code, err.message
                    )));
                }
                let part = store.upsert_imported(connection_id, &set)?;
                stats.inserted += part.inserted;
                stats.updated += part.updated;
                stats.skipped_tombstone += part.skipped_tombstone;
                stats.matched_pending += part.matched_pending;
            }
            Err(e) => {
                let msg = e.as_user_message();
                store.set_sync_result(connection_id, Some(&msg))?;
                return Err(e);
            }
        }
    }
    store.auto_categorize_new()?;
    let err_joined = if errors.is_empty() {
        None
    } else {
        Some(errors.join("\n"))
    };
    store.set_sync_result(connection_id, err_joined.as_deref())?;
    Ok(SyncReport { stats, errors })
}

fn windows_fallback(_start: i64, _end: i64) {}

/// Inclusive start / exclusive end covering [`DEFAULT_HISTORY_DAYS`].
pub fn default_history_window() -> DateWindow {
    let end = chrono::Utc::now().timestamp();
    let start = end - DEFAULT_HISTORY_DAYS * 86400;
    DateWindow {
        start_date: start,
        end_date: end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        chunk_windows, AccountSet, ConnectionSecrets, MapTransport, NormalizedAccount,
        NormalizedTxn, SimpleFinSource, TransportResponse, MAX_WINDOW_DAYS, OVERLAP_DAYS,
    };
    use crate::TxnPatch;
    use tempfile::tempdir;

    #[test]
    fn default_history_chunks_stay_within_bridge_recommendation() {
        let w = default_history_window();
        assert_eq!(w.end_date - w.start_date, DEFAULT_HISTORY_DAYS * 86400);
        let chunks = chunk_windows(w.start_date, w.end_date, MAX_WINDOW_DAYS, OVERLAP_DAYS);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.end_date - c.start_date <= MAX_WINDOW_DAYS * 86400);
        }
    }

    fn sample_set(remote_id: &str, pending: bool, amount: i64, desc: &str) -> AccountSet {
        AccountSet {
            errors: vec![],
            accounts: vec![NormalizedAccount {
                remote_id: "acct".into(),
                conn_id: "c".into(),
                name: "Checking".into(),
                institution: None,
                currency: "USD".into(),
                balance_cents: 0,
                available_cents: None,
                balance_date: 1_700_000_000,
                transactions: vec![NormalizedTxn {
                    remote_id: remote_id.into(),
                    posted: 1_700_000_100,
                    transacted_at: None,
                    amount_cents: amount,
                    description: desc.into(),
                    pending,
                    raw: None,
                }],
                raw: None,
            }],
        }
    }

    #[test]
    fn dedup_same_triple() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        let set = sample_set("tx1", false, -100, "Shop");
        store.upsert_imported(&cid, &set).unwrap();
        store.upsert_imported(&cid, &set).unwrap();
        assert_eq!(store.list_transactions(false, None).unwrap().len(), 1);
    }

    #[test]
    fn tombstone_prevents_resurrect() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", false, -100, "Shop"))
            .unwrap();
        let id = store.list_transactions(false, None).unwrap()[0].id.clone();
        store.tombstone_and_delete(&id).unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", false, -100, "Shop"))
            .unwrap();
        assert!(store.list_transactions(false, None).unwrap().is_empty());
    }

    #[test]
    fn pending_same_id_updates() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", true, -500, "Pending Coffee"))
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", false, -500, "Coffee"))
            .unwrap();
        let txns = store.list_transactions(false, None).unwrap();
        assert_eq!(txns.len(), 1);
        assert!(!txns[0].pending);
    }

    #[test]
    fn pending_new_id_matches() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("pend1", true, -500, "Coffee Shop"))
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("post9", false, -500, "Coffee Shop"))
            .unwrap();
        let txns = store.list_transactions(false, None).unwrap();
        assert_eq!(txns.len(), 1);
        assert!(!txns[0].pending);
    }

    #[test]
    fn user_override_survives_sync() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", false, -100, "ATM WITHDRAWAL"))
            .unwrap();
        let id = store.list_transactions(false, None).unwrap()[0].id.clone();
        store
            .patch_transactions(
                &[id],
                &TxnPatch {
                    payee: Some("Cash".into()),
                    amount_cents: Some(-120),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .upsert_imported(&cid, &sample_set("tx1", false, -100, "ATM WITHDRAWAL"))
            .unwrap();
        let txn = &store.list_transactions(false, None).unwrap()[0];
        assert_eq!(txn.payee, "Cash");
        assert_eq!(txn.amount_cents, -120);
    }

    #[test]
    fn errlist_surfaced() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path(), "pass").unwrap();
        let cid = store
            .add_connection(
                "simplefin",
                "SF",
                &ConnectionSecrets {
                    inner: "https://demo:pass@bridge.simplefin.org/simplefin".into(),
                },
            )
            .unwrap();
        let mut t = MapTransport::new();
        t.by_url.insert(
            "https://bridge.simplefin.org/simplefin/accounts".into(),
            TransportResponse {
                status: 200,
                body: include_bytes!("../tests/fixtures/simplefin_accounts.json").to_vec(),
            },
        );
        let report = sync_connection(&store, &cid, &SimpleFinSource, &t, 1, 2).unwrap();
        assert!(!report.errors.is_empty());
        assert!(report.errors[0].contains("act.failed"));
        assert!(!report.errors[0].contains('<'));
    }

    #[test]
    fn claim_https_only_via_transport() {
        let t = MapTransport::new();
        let token = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"http://example.com/claim",
        );
        assert!(SimpleFinSource.claim(&token, &t).is_err());
    }
}
