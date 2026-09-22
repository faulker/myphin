//! The AI pass: ask the configured provider about every row rules and memory left
//! uncategorized, one call per distinct payee, and apply answers above the threshold.

use std::collections::{BTreeMap, HashSet};

use super::debug::RecordingTransport;
use super::{categorizer_for, AiSettings, CategorizeInput, Categorizer, CategoryOption, Direction};
use crate::domain::normalize_payee;
use crate::error::Error;
use crate::providers::Transport;
use crate::store::Store;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct AiRunReport {
    /// Distinct payees looked at (cache hits included).
    pub payees: u32,
    /// Payees that needed a network call.
    pub asked: u32,
    /// Rows that got a category.
    pub categorized: u32,
    /// Payees whose best answer was "other" or fell under the threshold.
    pub below_threshold: u32,
    /// Sanitized provider error that stopped the run early, if any.
    pub error: Option<String>,
}

impl AiRunReport {
    /// One-line toast text.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "AI categorized {} row{} across {} payee{}",
            self.categorized,
            if self.categorized == 1 { "" } else { "s" },
            self.payees,
            if self.payees == 1 { "" } else { "s" }
        );
        if self.below_threshold > 0 {
            s.push_str(&format!(", {} below threshold", self.below_threshold));
        }
        s.push('.');
        if let Some(e) = &self.error {
            s.push(' ');
            s.push_str(e);
        }
        s
    }
}

/// Run the pass over every candidate row with whatever is configured in Setup → AI, the
/// Setup → AI button. Errors when nothing is set up. Rows are grouped by normalized payee;
/// cached answers skip the network. Rows whose last attempt failed are retried here. A
/// provider error stops the run but keeps what was already applied.
pub fn categorize_uncategorized(
    store: &Store,
    transport: &dyn Transport,
) -> Result<AiRunReport, Error> {
    let rows = store.list_ai_candidates(true)?;
    categorize_rows(store, transport, rows)
}

/// The same pass limited to the given transaction ids, for one row or the rows on screen.
/// Ids that are not candidates (already categorized, income, transfer, excluded, hidden) are
/// skipped, so the report can come back with zero payees. Naming an id is an explicit ask, so
/// rows flagged as failed are retried. Answers apply only to these ids, never to other rows
/// sharing the payee.
pub fn categorize_txns(
    store: &Store,
    transport: &dyn Transport,
    ids: &[String],
) -> Result<AiRunReport, Error> {
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let rows = store
        .list_ai_candidates(true)?
        .into_iter()
        .filter(|(id, _, _)| wanted.contains(id.as_str()))
        .collect();
    categorize_rows(store, transport, rows)
}

/// Settings, provider, and option list the pass needs, or a user-facing error when any of
/// them is missing. Shared with the debug trace so both see the same thing.
pub(super) fn ai_context(
    store: &Store,
) -> Result<(AiSettings, &'static dyn Categorizer, Vec<CategoryOption>), Error> {
    let settings = store.ai_settings()?;
    if !settings.is_configured() {
        return Err(Error::user("Add an AI key in Setup → AI first."));
    }
    let provider_id = settings.provider.as_deref().unwrap_or_default();
    let categorizer = categorizer_for(provider_id)
        .ok_or_else(|| Error::user("That AI provider is not available in this build."))?;
    let cats = store.list_categories()?;
    if cats.is_empty() {
        return Err(Error::user("Add at least one category before using AI."));
    }
    let options: Vec<CategoryOption> = cats
        .into_iter()
        .filter(|c| c.is_sent_to_ai())
        .map(|c| CategoryOption {
            id: c.id.clone(),
            // A sub-category goes as "Parent › Child" so the model sees where it sits.
            name: c.label(),
            description: c.description.clone(),
        })
        .collect();
    if options.is_empty() {
        return Err(Error::user(
            "Turn on Send to AI for at least one category first.",
        ));
    }
    Ok((settings, categorizer, options))
}

/// Shared body of the two entry points. `rows` are (id, payee, amount_cents) candidates.
fn categorize_rows(
    store: &Store,
    transport: &dyn Transport,
    rows: Vec<(String, String, i64)>,
) -> Result<AiRunReport, Error> {
    let (settings, categorizer, options) = ai_context(store)?;
    let provider_id = settings.provider.as_deref().unwrap_or_default();

    // Group rows by payee key; the first (newest) row's title and sign represent the group.
    let mut groups: BTreeMap<String, (String, i64, Vec<String>)> = BTreeMap::new();
    for (id, payee, amount) in rows {
        let key = normalize_payee(&payee);
        if key.is_empty() {
            continue;
        }
        groups
            .entry(key)
            .or_insert_with(|| (payee, amount, Vec::new()))
            .2
            .push(id);
    }

    let mut report = AiRunReport::default();
    for (key, (title, amount, ids)) in groups {
        report.payees += 1;
        let answer = match store.ai_answer(&key)? {
            Some(cached) => cached,
            None => {
                report.asked += 1;
                let input = CategorizeInput {
                    title,
                    direction: Direction::from_amount(amount),
                };
                // Record the round trip so Activity's debug view can show exactly what was
                // sent and answered for this payee. Headers (the key) are never kept.
                let recorder = RecordingTransport::new(transport);
                let outcome = categorizer.categorize(&settings, &input, &options, &recorder);
                let exchanges = recorder.into_exchanges();
                match outcome {
                    Ok(guess) => {
                        store.remember_ai_answer(
                            &key,
                            guess.category_id.as_deref(),
                            guess.confidence,
                            provider_id,
                            &exchanges,
                        )?;
                        (guess.category_id, guess.confidence)
                    }
                    Err(e) => {
                        // Keep the failed round trip for the trace and flag the rows so the
                        // after-sync pass stops retrying them until asked to.
                        let msg = e.as_user_message();
                        store.remember_ai_failure(&key, provider_id, &msg, &exchanges)?;
                        store.mark_ai_failed(&ids)?;
                        report.error = Some(msg);
                        break;
                    }
                }
            }
        };
        store.clear_ai_failed(&ids)?;
        match answer {
            (Some(cat), confidence) if confidence >= settings.threshold => {
                report.categorized += store.apply_ai_category(&ids, &cat)?;
            }
            _ => report.below_threshold += 1,
        }
    }
    store.persist()?;
    Ok(report)
}

/// The after-sync hook. `None` when the toggle is off or nothing is configured. Rows whose
/// last attempt failed are left alone; only "Ask AI" or the Setup → AI button retry them.
pub fn run_after_sync(
    store: &Store,
    transport: &dyn Transport,
) -> Result<Option<AiRunReport>, Error> {
    let settings = store.ai_settings()?;
    if !settings.after_sync || !settings.is_configured() {
        return Ok(None);
    }
    let rows = store.list_ai_candidates(false)?;
    categorize_rows(store, transport, rows).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSecret, AiSettings};
    use crate::providers::{
        AccountSet, ConnectionSecrets, NormalizedAccount, NormalizedTxn, ScriptedTransport,
        TransportResponse,
    };
    use crate::{Rule, RuleAction, TxnPatch, TxnQuery};
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let s = Store::open(dir.path(), "pass").unwrap();
        (dir, s)
    }

    fn seed(s: &Store, rows: &[(&str, i64)]) -> Vec<String> {
        let cid = s
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        let txns = rows
            .iter()
            .enumerate()
            .map(|(i, (desc, amount))| NormalizedTxn {
                remote_id: format!("t{i}"),
                posted: 1_700_000_000 + i as i64,
                transacted_at: None,
                amount_cents: *amount,
                description: desc.to_string(),
                pending: false,
                raw: None,
            })
            .collect();
        s.upsert_imported(
            &cid,
            &AccountSet {
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
                    transactions: txns,
                    raw: None,
                }],
            },
        )
        .unwrap();
        let mut all = s.list_transactions(false, None).unwrap();
        all.sort_by_key(|t| t.posted_at);
        all.into_iter().map(|t| t.id).collect()
    }

    fn configure(s: &Store, threshold: f64, after_sync: bool) {
        s.set_ai_settings(&AiSettings {
            provider: Some("typesafe".into()),
            api_key: AiSecret("k".into()),
            threshold,
            after_sync,
            ..Default::default()
        })
        .unwrap();
    }

    fn ok(choice: &str, confidence: f64) -> TransportResponse {
        TransportResponse {
            status: 200,
            body: format!(
                r#"{{"answers":{{"category":{{"type":"choice","choice":"{choice}","probabilities":{{}},"confidence":{confidence}}}}}}}"#
            )
            .into_bytes(),
        }
    }

    fn categorized_by(s: &Store, id: &str) -> Option<String> {
        s.conn()
            .query_row(
                "SELECT categorized_by FROM transactions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn unconfigured_is_a_user_error() {
        let (_d, s) = store();
        s.add_category("Food").unwrap();
        let t = ScriptedTransport::new(vec![]);
        let e = categorize_uncategorized(&s, &t).unwrap_err();
        assert!(e.as_user_message().contains("Setup"));
        assert_eq!(t.calls(), 0);
        assert!(run_after_sync(&s, &t).unwrap().is_none());
    }

    #[test]
    fn one_call_per_payee_and_threshold_applies() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        s.set_category_description(&food, "Groceries and restaurants")
            .unwrap();
        s.add_category("Gas").unwrap();
        configure(&s, 0.70, false);
        let ids = seed(
            &s,
            &[
                ("COSTCO WHSE #123", -5000),
                ("COSTCO WHSE #456", -2000),
                ("SHELL OIL", -3000),
                ("MYSTERY CO", -100),
            ],
        );
        // Newest first: MYSTERY, SHELL, COSTCO (BTreeMap orders by key: costco, mystery co, shell oil).
        let t = ScriptedTransport::new(vec![ok("Food", 0.91), ok("other", 0.9), ok("Gas", 0.5)]);
        let r = categorize_uncategorized(&s, &t).unwrap();
        assert_eq!(t.calls(), 3);
        assert_eq!(r.payees, 3);
        assert_eq!(r.asked, 3);
        assert_eq!(r.categorized, 2);
        assert_eq!(r.below_threshold, 2);
        assert!(r.error.is_none());
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("ai"));
        assert_eq!(categorized_by(&s, &ids[1]).as_deref(), Some("ai"));
        assert_eq!(categorized_by(&s, &ids[2]), None);
        assert_eq!(categorized_by(&s, &ids[3]), None);
        // The request carried the description and the direction.
        let body = String::from_utf8(t.requests.lock().unwrap()[0].2.clone()).unwrap();
        assert!(body.contains("Groceries and restaurants"));
        assert!(body.contains("money out"));

        // The review filter lists exactly the AI rows, and Txn exposes provenance.
        let ai_rows = s
            .query_transactions(&TxnQuery {
                ai_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(ai_rows.len(), 2);
        assert!(ai_rows
            .iter()
            .all(|t| t.categorized_by.as_deref() == Some("ai")));

        // A second run hits the cache: no network, and a lower threshold now accepts Gas.
        s.set_ai_settings(&AiSettings {
            threshold: 0.40,
            ..s.ai_settings().unwrap()
        })
        .unwrap();
        let t2 = ScriptedTransport::new(vec![]);
        let r2 = categorize_uncategorized(&s, &t2).unwrap();
        assert_eq!(t2.calls(), 0);
        assert_eq!(r2.asked, 0);
        assert_eq!(r2.categorized, 1);
        assert_eq!(categorized_by(&s, &ids[2]).as_deref(), Some("ai"));
        assert_eq!(
            r2.summary(),
            "AI categorized 1 row across 2 payees, 1 below threshold."
        );
    }

    #[test]
    fn run_records_the_exchange_for_each_payee() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        configure(&s, 0.70, false);
        s.set_ai_settings(&AiSettings {
            api_key: AiSecret("sk-secret-key-xyz".into()),
            ..s.ai_settings().unwrap()
        })
        .unwrap();
        let ids = seed(
            &s,
            &[("COSTCO WHSE #123", -5000), ("COSTCO WHSE #456", -2000)],
        );
        let t = ScriptedTransport::new(vec![ok("Food", 0.91)]);
        categorize_uncategorized(&s, &t).unwrap();

        // Both rows share the payee, so both show the same recorded round trip.
        for id in &ids {
            let rec = s.ai_record_for(id).unwrap().expect("recorded");
            assert_eq!(rec.category_id.as_deref(), Some(food.as_str()));
            assert_eq!(rec.category_name.as_deref(), Some("Food"));
            assert_eq!(rec.provider, "typesafe");
            assert!((rec.confidence - 0.91).abs() < 1e-9);
            assert_eq!(rec.exchanges.len(), 1);
            let x = &rec.exchanges[0];
            assert_eq!(x.method, "POST");
            assert!(x.url.starts_with("https://"));
            assert_eq!(x.status, Some(200));
            assert!(x.request.contains("money out"));
            assert!(x.response.contains("\"choice\": \"Food\""));
            // Headers are never recorded, so the key cannot leak into the trace.
            assert!(!x.request.contains("sk-secret-key-xyz"));
            assert!(!x.url.contains("sk-secret-key-xyz"));
        }

        // A payee never asked has no record, and a gone row has none either.
        seed(&s, &[("NEVER ASKED", -1)]);
        let other = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.payee == "NEVER ASKED")
            .unwrap();
        assert!(s.ai_record_for(&other.id).unwrap().is_none());
        assert!(s.ai_record_for("nope").unwrap().is_none());

        // An answer cached before exchanges were kept reads back with an empty trail.
        s.conn()
            .execute(
                "UPDATE ai_answers SET exchanges=NULL WHERE normalized_payee=?1",
                [normalize_payee("COSTCO WHSE #123")],
            )
            .unwrap();
        assert!(s
            .ai_record_for(&ids[0])
            .unwrap()
            .unwrap()
            .exchanges
            .is_empty());
        // Such an answer is not treated as cached: the next ask goes to the network and
        // records the trail, so the trace has something to show.
        assert!(s.ai_answer("costco whse").unwrap().is_none());
        s.conn()
            .execute(
                "UPDATE transactions SET category_id=NULL, categorized_by=NULL WHERE id=?1",
                [&ids[0]],
            )
            .unwrap();
        let t2 = ScriptedTransport::new(vec![ok("Food", 0.93)]);
        categorize_txns(&s, &t2, std::slice::from_ref(&ids[0])).unwrap();
        assert_eq!(t2.calls(), 1);
        assert_eq!(
            s.ai_record_for(&ids[0]).unwrap().unwrap().exchanges.len(),
            1
        );
    }

    #[test]
    fn rules_memory_and_hand_edits_win_over_ai() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        let gas = s.add_category("Gas").unwrap();
        configure(&s, 0.70, false);
        let ids = seed(&s, &[("COSTCO WHSE", -5000), ("ARCO", -3000)]);
        let t = ScriptedTransport::new(vec![ok("Food", 0.9), ok("Food", 0.9)]);
        // BTreeMap key order: "arco" then "costco whse".
        let r = categorize_uncategorized(&s, &t).unwrap();
        assert_eq!(r.categorized, 2);
        assert_eq!(categorized_by(&s, &ids[1]).as_deref(), Some("ai"));

        // A rule added later takes the ARCO row away from the AI.
        s.create_rule(&Rule::pattern(
            "*arco*",
            RuleAction::Category,
            Some(gas.clone()),
            1,
        ))
        .unwrap();
        s.auto_categorize_new().unwrap();
        assert_eq!(categorized_by(&s, &ids[1]).as_deref(), Some("rule"));
        let row = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == ids[1])
            .unwrap();
        assert_eq!(row.category_id.as_deref(), Some(gas.as_str()));

        // A hand edit clears the ai tag, and AI rows never seed payee memory.
        s.patch_transactions(
            &[ids[0].clone()],
            &TxnPatch {
                category_id: Some(Some(food.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("user"));
        let mem: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM payee_memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mem, 1);
    }

    #[test]
    fn parent_and_subcategory_are_both_sent_and_child_answers_land_on_the_child() {
        let (_d, s) = store();
        let parent = s.add_category("Investment").unwrap();
        s.set_category_description(&parent, "brokerage activity")
            .unwrap();
        let fees = s.add_subcategory(&parent, "Fees").unwrap();
        s.set_category_description(&fees, "commissions and account fees")
            .unwrap();
        s.add_subcategory(&parent, "Interest").unwrap();
        let ids = seed(&s, &[("BROKER FEE", -900)]);
        configure(&s, 0.5, false);

        let t = ScriptedTransport::new(vec![ok("Investment › Fees", 0.9)]);
        categorize_uncategorized(&s, &t).unwrap();

        // The request lists the parent and each sub-category as its own choice, each with
        // its own description; the sub-category goes by its full label.
        let body: serde_json::Value =
            serde_json::from_slice(&t.requests.lock().unwrap()[0].2).unwrap();
        let criteria = &body["questions"]["category"]["criteria"];
        assert_eq!(criteria["Investment"], "brokerage activity");
        assert_eq!(
            criteria["Investment › Fees"],
            "commissions and account fees"
        );
        assert!(criteria["Investment › Interest"].is_null());
        assert!(criteria.get("Fees").is_none());
        assert!(criteria.get("Interest").is_none());
        assert!(criteria.get("other").is_some());

        let row = s.list_transactions(false, None).unwrap();
        let row = row.iter().find(|t| t.id == ids[0]).unwrap();
        assert_eq!(row.category_id.as_deref(), Some(fees.as_str()));
        assert_eq!(row.category_name.as_deref(), Some("Investment › Fees"));
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("ai"));
    }

    #[test]
    fn provider_error_stops_but_keeps_progress() {
        let (_d, s) = store();
        s.add_category("Food").unwrap();
        configure(&s, 0.70, true);
        let ids = seed(&s, &[("AAA MARKET", -100), ("BBB DINER", -200)]);
        let t = ScriptedTransport::new(vec![
            ok("Food", 0.9),
            TransportResponse {
                status: 401,
                body: vec![],
            },
        ]);
        let r = run_after_sync(&s, &t).unwrap().unwrap();
        assert_eq!(r.categorized, 1);
        assert_eq!(r.error.as_deref(), Some("AI key was rejected."));
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("ai"));
        assert_eq!(categorized_by(&s, &ids[1]), None);
        assert!(r.summary().ends_with("AI key was rejected."));
    }

    fn ai_failed_at(s: &Store, id: &str) -> Option<i64> {
        s.conn()
            .query_row(
                "SELECT ai_failed_at FROM transactions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn failed_call_is_recorded_flagged_and_skipped_after_sync() {
        let (_d, s) = store();
        s.add_category("Food").unwrap();
        configure(&s, 0.70, true);
        let ids = seed(&s, &[("BBB DINER", -200), ("BBB DINER", -300)]);
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 401,
            body: vec![],
        }]);
        let r = run_after_sync(&s, &t).unwrap().unwrap();
        assert_eq!(r.error.as_deref(), Some("AI key was rejected."));

        // Both rows of the payee are flagged, and the trace shows the failed round trip.
        assert!(ids.iter().all(|id| ai_failed_at(&s, id).is_some()));
        let rec = s.ai_record_for(&ids[0]).unwrap().expect("recorded");
        assert_eq!(rec.error.as_deref(), Some("AI key was rejected."));
        assert_eq!(rec.exchanges.len(), 1);
        assert_eq!(rec.exchanges[0].status, Some(401));
        // A failure is not a cached answer.
        assert!(s.ai_answer("bbb diner").unwrap().is_none());

        // The after-sync pass leaves flagged rows alone: no network call at all.
        let t2 = ScriptedTransport::new(vec![]);
        let r2 = run_after_sync(&s, &t2).unwrap().unwrap();
        assert_eq!(t2.calls(), 0);
        assert_eq!(r2.payees, 0);
        assert!(ids.iter().all(|id| ai_failed_at(&s, id).is_some()));

        // "Ask AI" on one row retries it and clears its flag; the sibling stays flagged.
        let t3 = ScriptedTransport::new(vec![ok("Food", 0.9)]);
        let r3 = categorize_txns(&s, &t3, std::slice::from_ref(&ids[0])).unwrap();
        assert_eq!(t3.calls(), 1);
        assert_eq!(r3.categorized, 1);
        assert!(ai_failed_at(&s, &ids[0]).is_none());
        assert!(ai_failed_at(&s, &ids[1]).is_some());
        let rec = s.ai_record_for(&ids[1]).unwrap().unwrap();
        assert!(rec.error.is_none());
        assert_eq!(rec.category_name.as_deref(), Some("Food"));

        // The Setup → AI button retries every flagged row; the cached answer applies.
        let t4 = ScriptedTransport::new(vec![]);
        let r4 = categorize_uncategorized(&s, &t4).unwrap();
        assert_eq!(t4.calls(), 0);
        assert_eq!(r4.categorized, 1);
        assert!(ai_failed_at(&s, &ids[1]).is_none());
    }

    #[test]
    fn candidates_list_honours_the_failure_flag() {
        let (_d, s) = store();
        let ids = seed(&s, &[("AAA", -1), ("BBB", -2)]);
        s.mark_ai_failed(std::slice::from_ref(&ids[0])).unwrap();
        let skip: Vec<String> = s
            .list_ai_candidates(false)
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(skip, vec![ids[1].clone()]);
        assert_eq!(s.list_ai_candidates(true).unwrap().len(), 2);
        s.clear_ai_failed(&ids).unwrap();
        assert_eq!(s.list_ai_candidates(false).unwrap().len(), 2);
    }

    #[test]
    fn category_changes_clear_the_cache() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        configure(&s, 0.70, false);
        seed(&s, &[("AAA MARKET", -100)]);
        let t = ScriptedTransport::new(vec![ok("other", 0.9)]);
        categorize_uncategorized(&s, &t).unwrap();
        assert!(s.ai_answer("aaa market").unwrap().is_some());
        s.set_category_description(&food, "Food things").unwrap();
        assert!(s.ai_answer("aaa market").unwrap().is_none());
        // Asked again now that the options changed.
        let t2 = ScriptedTransport::new(vec![ok("Food", 0.95)]);
        let r = categorize_uncategorized(&s, &t2).unwrap();
        assert_eq!(t2.calls(), 1);
        assert_eq!(r.categorized, 1);
        s.rename_category(&food, "Eats").unwrap();
        assert!(s.ai_answer("aaa market").unwrap().is_none());
        // Send-to-AI also changes the option list, so it clears the cache too.
        seed(&s, &[("BBB DINER", -100)]);
        let t3 = ScriptedTransport::new(vec![ok("Eats", 0.95)]);
        categorize_uncategorized(&s, &t3).unwrap();
        assert!(s.ai_answer("bbb diner").unwrap().is_some());
        s.set_category_send_to_ai(&food, false).unwrap();
        assert!(s.ai_answer("bbb diner").unwrap().is_none());
    }

    #[test]
    fn hidden_categories_are_not_sent_and_all_hidden_is_an_error() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        s.set_category_description(&food, "Groceries and restaurants")
            .unwrap();
        let other = s.add_category("Other").unwrap();
        s.set_category_send_to_ai(&other, false).unwrap();
        configure(&s, 0.70, false);
        seed(&s, &[("COSTCO WHSE", -5000)]);

        let t = ScriptedTransport::new(vec![ok("Food", 0.91)]);
        categorize_uncategorized(&s, &t).unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&t.requests.lock().unwrap()[0].2).unwrap();
        let criteria = &body["questions"]["category"]["criteria"];
        assert_eq!(criteria["Food"], "Groceries and restaurants");
        assert!(criteria.get("Other").is_none());

        s.set_category_send_to_ai(&food, false).unwrap();
        let t2 = ScriptedTransport::new(vec![]);
        let e = categorize_uncategorized(&s, &t2).unwrap_err();
        assert!(e.as_user_message().contains("Send to AI"));
        assert_eq!(t2.calls(), 0);
    }

    #[test]
    fn parent_off_omits_its_children_from_the_option_list() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        s.add_subcategory(&food, "Dining").unwrap();
        s.add_category("Gas").unwrap();
        configure(&s, 0.70, false);
        seed(&s, &[("CAFE", -500)]);
        s.set_category_send_to_ai(&food, false).unwrap();

        let t = ScriptedTransport::new(vec![ok("Gas", 0.91)]);
        categorize_uncategorized(&s, &t).unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&t.requests.lock().unwrap()[0].2).unwrap();
        let criteria = &body["questions"]["category"]["criteria"];
        assert!(criteria.get("Food").is_none());
        assert!(criteria.get("Food › Dining").is_none());
        assert!(criteria.get("Gas").is_some());
    }

    #[test]
    fn skips_flagged_and_hidden_rows() {
        let (_d, s) = store();
        s.add_category("Food").unwrap();
        configure(&s, 0.70, false);
        let ids = seed(&s, &[("AAA MARKET", -100), ("PAYROLL", 5000)]);
        s.patch_transactions(
            &[ids[1].clone()],
            &TxnPatch {
                income: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let t = ScriptedTransport::new(vec![ok("Food", 0.9)]);
        let r = categorize_uncategorized(&s, &t).unwrap();
        assert_eq!(r.payees, 1);
        assert_eq!(t.calls(), 1);
        assert_eq!(categorized_by(&s, &ids[1]), None);
    }

    #[test]
    fn categorize_txns_only_touches_the_given_rows() {
        let (_d, s) = store();
        s.add_category("Food").unwrap();
        configure(&s, 0.70, false);
        let ids = seed(
            &s,
            &[
                ("COSTCO WHSE #123", -5000),
                ("COSTCO WHSE #456", -2000),
                ("SHELL OIL", -3000),
            ],
        );
        // One row asked for: one call, one row categorized. The sibling COSTCO row and SHELL
        // stay untouched even though the payee answer is now cached.
        let t = ScriptedTransport::new(vec![ok("Food", 0.95)]);
        let r = categorize_txns(&s, &t, std::slice::from_ref(&ids[0])).unwrap();
        assert_eq!(t.calls(), 1);
        assert_eq!(r.payees, 1);
        assert_eq!(r.categorized, 1);
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("ai"));
        assert_eq!(categorized_by(&s, &ids[1]), None);
        assert_eq!(categorized_by(&s, &ids[2]), None);
        assert!(s.ai_answer("costco whse").unwrap().is_some());
    }

    #[test]
    fn categorize_txns_skips_non_candidates_and_uses_cache() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        configure(&s, 0.70, false);
        let ids = seed(&s, &[("SHELL OIL", -5000), ("COSTCO WHSE #456", -2000)]);
        // Hand-categorized rows are not candidates, so asking for one is a no-op.
        s.patch_transactions(
            std::slice::from_ref(&ids[0]),
            &TxnPatch {
                category_id: Some(Some(food.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        let t = ScriptedTransport::new(vec![]);
        let r = categorize_txns(&s, &t, std::slice::from_ref(&ids[0])).unwrap();
        assert_eq!(t.calls(), 0);
        assert_eq!(r.payees, 0);
        assert_eq!(categorized_by(&s, &ids[0]).as_deref(), Some("user"));
        // A cached answer for the payee applies to the other row without a call.
        s.remember_ai_answer("costco whse", Some(&food), 0.9, "typesafe", &[])
            .unwrap();
        let r = categorize_txns(&s, &t, &ids).unwrap();
        assert_eq!(t.calls(), 0);
        assert_eq!(r.payees, 1);
        assert_eq!(r.asked, 0);
        assert_eq!(r.categorized, 1);
        assert_eq!(categorized_by(&s, &ids[1]).as_deref(), Some("ai"));
    }
}
