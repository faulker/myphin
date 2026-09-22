//! Debug trace for one transaction: run the configured provider on it and capture exactly
//! what went over the wire, in both directions. Nothing is cached or applied, so the tool can
//! be pointed at any row (categorized or not) without touching the ledger. Headers are never
//! recorded, so the trace is safe to show on screen.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::run::ai_context;
use super::{CategorizeInput, CategoryGuess, Direction};
use crate::error::Error;
use crate::providers::{Transport, TransportRequest, TransportResponse};
use crate::store::Store;

/// One HTTP round trip as the provider made it. A retry produces a second exchange.
/// Serialized as JSON into `ai_answers.exchanges` when a real run records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiExchange {
    pub method: String,
    pub url: String,
    /// Request body, pretty-printed when it is JSON.
    pub request: String,
    /// `None` when the transport itself failed before a status came back.
    pub status: Option<u16>,
    /// Response body, pretty-printed when it is JSON. Empty when nothing came back.
    pub response: String,
}

/// Everything the debug card shows for one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct AiTrace {
    pub input: CategorizeInput,
    pub exchanges: Vec<AiExchange>,
    /// The provider's parsed answer, when the call succeeded.
    pub guess: Option<CategoryGuess>,
    /// Name of the picked category, looked up from the ledger. `None` for "other".
    pub category_name: Option<String>,
    /// Criteria key and probability of the highest entry in
    /// `answers.category.probabilities`. `None` when the call failed or the response had no
    /// distribution. This is the rating, separate from `guess.confidence`.
    pub top_rating: Option<(String, f64)>,
    /// Wall time of the provider call, including retries.
    pub elapsed: Duration,
    /// The sanitized error that ended the call, if any.
    pub error: Option<String>,
}

impl AiTrace {
    /// One line for the debug card: the highest-rated category when the response included a
    /// distribution, the pick and its confidence, and how long the call took.
    pub fn outcome(&self) -> String {
        let took = format_elapsed(self.elapsed);
        if let Some(e) = &self.error {
            return format!("Failed: {e} Took {took}.");
        }
        let Some(g) = &self.guess else {
            return format!("No answer. Took {took}.");
        };
        let conf = percent(g.confidence);
        let picked = match &self.category_name {
            Some(name) => format!("Picked {name} at {conf}% confidence."),
            None => format!("Picked other (nothing fits) at {conf}% confidence."),
        };
        match &self.top_rating {
            Some((name, p)) if rating_is_the_pick(name, self) => {
                format!(
                    "Highest: {name} at {}%. Confidence {conf}%. Took {took}.",
                    percent(*p)
                )
            }
            Some((name, p)) => {
                format!("Highest: {name} at {}%. {picked} Took {took}.", percent(*p))
            }
            None => format!("{picked} Took {took}."),
        }
    }
}

/// True when the highest-rated criteria key is the category the parser picked.
fn rating_is_the_pick(name: &str, trace: &AiTrace) -> bool {
    match &trace.category_name {
        Some(picked) => picked == name,
        None => name.eq_ignore_ascii_case("other"),
    }
}

/// Whole percent, same rounding as the rest of the AI copy.
fn percent(p: f64) -> i64 {
    (p * 100.0).round() as i64
}

/// "340 ms" under a second, "1.2 s" under ten, then whole seconds.
fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        let secs = d.as_secs_f64();
        if secs < 10.0 {
            format!("{secs:.1} s")
        } else {
            format!("{secs:.0} s")
        }
    }
}

/// Highest finite probability in a System One choice answer, or `None` when there is no
/// distribution. On a tie, the model's `choice` wins so the line matches what it picked.
fn highest_rating(body: &str) -> Option<(String, f64)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let answer = v.pointer("/answers/category")?;
    let probs = answer.get("probabilities")?.as_object()?;
    let mut best: Option<(String, f64)> = None;
    for (key, val) in probs {
        let Some(p) = val.as_f64().filter(|p| p.is_finite()) else {
            continue;
        };
        match &best {
            Some((_, bp)) if p <= *bp => {}
            _ => best = Some((key.clone(), p)),
        }
    }
    let (name, p) = best?;
    if let Some(choice) = answer.get("choice").and_then(|c| c.as_str()) {
        if let Some(cp) = probs.get(choice).and_then(|v| v.as_f64()) {
            if cp.is_finite() && (cp - p).abs() < 1e-9 {
                return Some((choice.to_string(), cp));
            }
        }
    }
    Some((name, p))
}

/// Wraps a real transport and keeps every request body and response body it sees. Only the
/// method, URL, and bodies are kept; headers (where the key lives) are dropped on purpose.
pub struct RecordingTransport<'a> {
    inner: &'a dyn Transport,
    exchanges: Mutex<Vec<AiExchange>>,
}

impl<'a> RecordingTransport<'a> {
    pub fn new(inner: &'a dyn Transport) -> Self {
        Self {
            inner,
            exchanges: Mutex::new(Vec::new()),
        }
    }

    /// Hand back what was recorded, oldest first.
    pub fn into_exchanges(self) -> Vec<AiExchange> {
        self.exchanges.into_inner().unwrap_or_default()
    }
}

impl Transport for RecordingTransport<'_> {
    fn send(&self, req: &TransportRequest) -> Result<TransportResponse, Error> {
        let result = self.inner.send(req);
        let (status, response) = match &result {
            Ok(r) => (Some(r.status), pretty_body(&r.body)),
            Err(_) => (None, String::new()),
        };
        self.exchanges.lock().unwrap().push(AiExchange {
            method: req.method.to_string(),
            url: req.url.clone(),
            request: req.body.as_deref().map(pretty_body).unwrap_or_default(),
            status,
            response,
        });
        result
    }
}

/// Pretty-print JSON bodies; anything else is shown as (lossy) text.
pub fn pretty_body(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Ask the configured provider about one transaction and return the full trace. Errors only
/// when the transaction is missing or nothing is set up; a provider failure comes back inside
/// the trace so the request that caused it can still be inspected.
pub fn trace_transaction(
    store: &Store,
    transport: &dyn Transport,
    txn_id: &str,
) -> Result<AiTrace, Error> {
    let (payee, amount) = store
        .ai_input_for(txn_id)?
        .ok_or_else(|| Error::user("That transaction is gone."))?;
    let (settings, categorizer, options) = ai_context(store)?;
    let input = CategorizeInput {
        title: payee,
        direction: Direction::from_amount(amount),
    };
    let recorder = RecordingTransport::new(transport);
    let started = Instant::now();
    let outcome = categorizer.categorize(&settings, &input, &options, &recorder);
    let elapsed = started.elapsed();
    let exchanges = recorder.into_exchanges();
    let (guess, error) = match outcome {
        Ok(g) => (Some(g), None),
        Err(e) => (None, Some(e.as_user_message())),
    };
    let category_name = guess
        .as_ref()
        .and_then(|g| g.category_id.as_deref())
        .and_then(|id| options.iter().find(|o| o.id == id).map(|o| o.name.clone()));
    let top_rating = exchanges.iter().rev().find_map(|x| match x.status {
        Some(status) if (200..300).contains(&status) => highest_rating(&x.response),
        _ => None,
    });
    Ok(AiTrace {
        input,
        exchanges,
        guess,
        category_name,
        top_rating,
        elapsed,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSecret, AiSettings};
    use crate::providers::{
        AccountSet, ConnectionSecrets, NormalizedAccount, NormalizedTxn, ScriptedTransport,
    };
    use tempfile::tempdir;

    fn store_with_row(payee: &str, amount: i64) -> (tempfile::TempDir, Store, String) {
        let dir = tempdir().unwrap();
        let s = Store::open(dir.path(), "pass").unwrap();
        let cid = s
            .add_connection(
                "mock",
                "M",
                &ConnectionSecrets {
                    inner: "https://x:y@example.com/s".into(),
                },
            )
            .unwrap();
        s.upsert_imported(
            &cid,
            &AccountSet {
                errors: vec![],
                accounts: vec![NormalizedAccount {
                    remote_id: "a1".into(),
                    conn_id: cid.clone(),
                    name: "Checking".into(),
                    institution: None,
                    currency: "USD".into(),
                    balance_cents: 0,
                    available_cents: None,
                    balance_date: 1_700_000_000,
                    raw: None,
                    transactions: vec![NormalizedTxn {
                        remote_id: "t1".into(),
                        posted: 1_700_000_000,
                        transacted_at: None,
                        amount_cents: amount,
                        description: payee.into(),
                        pending: false,
                        raw: None,
                    }],
                }],
            },
        )
        .unwrap();
        let id = s.list_transactions(false, None).unwrap()[0].id.clone();
        (dir, s, id)
    }

    fn configure(s: &Store) {
        s.set_ai_settings(&AiSettings {
            provider: Some("typesafe".into()),
            api_key: AiSecret("sk-secret".into()),
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn trace_captures_request_and_response_without_the_key() {
        let (_d, s, id) = store_with_row("STARBUCKS 123", -450);
        configure(&s);
        let cat = s.add_category("Dining").unwrap();
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: br#"{"answers":{"category":{"choice":"Dining","probabilities":{"Gas":0.2,"Dining":0.71,"other":0.09},"confidence":0.91}}}"#
                .to_vec(),
        }]);
        let trace = trace_transaction(&s, &t, &id).unwrap();
        assert_eq!(trace.input.title, "STARBUCKS 123");
        assert_eq!(trace.input.direction, Direction::Out);
        assert_eq!(trace.exchanges.len(), 1);
        let x = &trace.exchanges[0];
        assert_eq!(x.method, "POST");
        assert!(x.url.starts_with("https://"));
        assert_eq!(x.status, Some(200));
        assert!(x
            .request
            .contains("\"transactionTitle\": \"STARBUCKS 123\""));
        assert!(x.request.contains("\"Dining\""));
        assert!(x.response.contains("\"confidence\": 0.91"));
        assert!(!x.request.contains("sk-secret"));
        assert!(!x.response.contains("sk-secret"));
        assert_eq!(
            trace.guess.as_ref().unwrap().category_id.as_deref(),
            Some(cat.as_str())
        );
        assert_eq!(trace.category_name.as_deref(), Some("Dining"));
        assert_eq!(
            trace.top_rating.as_ref().map(|(n, _)| n.as_str()),
            Some("Dining")
        );
        assert!((trace.top_rating.as_ref().unwrap().1 - 0.71).abs() < 1e-9);
        assert!(trace.elapsed < Duration::from_secs(5));
        assert_eq!(
            trace.outcome(),
            format!(
                "Highest: Dining at 71%. Confidence 91%. Took {}.",
                format_elapsed(trace.elapsed)
            )
        );
        assert!(trace.error.is_none());
        // Debugging must not write anything: no cached answer, row still uncategorized.
        assert!(s.ai_answer("starbucks").unwrap().is_none());
        assert!(s.list_transactions(false, None).unwrap()[0]
            .category_id
            .is_none());
    }

    #[test]
    fn trace_keeps_the_request_when_the_provider_fails() {
        let (_d, s, id) = store_with_row("ACME", -100);
        configure(&s);
        s.add_category("Stuff").unwrap();
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 500,
            body: b"boom".to_vec(),
        }]);
        let trace = trace_transaction(&s, &t, &id).unwrap();
        assert_eq!(trace.exchanges.len(), 1);
        assert_eq!(trace.exchanges[0].status, Some(500));
        assert_eq!(trace.exchanges[0].response, "boom");
        assert!(trace.guess.is_none());
        assert_eq!(
            trace.error.as_deref(),
            Some("AI service returned HTTP 500.")
        );
        assert!(trace.top_rating.is_none());
        assert!(trace.elapsed < Duration::from_secs(5));
        assert_eq!(
            trace.outcome(),
            format!(
                "Failed: AI service returned HTTP 500. Took {}.",
                format_elapsed(trace.elapsed)
            )
        );
    }

    fn sample(elapsed: Duration) -> AiTrace {
        AiTrace {
            input: CategorizeInput {
                title: "STARBUCKS".into(),
                direction: Direction::Out,
            },
            exchanges: vec![],
            guess: Some(CategoryGuess {
                category_id: Some("c1".into()),
                confidence: 0.91,
            }),
            category_name: Some("Dining".into()),
            top_rating: Some(("Dining".into(), 0.71)),
            elapsed,
            error: None,
        }
    }

    #[test]
    fn outcome_names_the_highest_rating_and_the_time() {
        let mut t = sample(Duration::from_millis(340));
        assert_eq!(
            t.outcome(),
            "Highest: Dining at 71%. Confidence 91%. Took 340 ms."
        );
        t.elapsed = Duration::from_millis(1500);
        assert_eq!(
            t.outcome(),
            "Highest: Dining at 71%. Confidence 91%. Took 1.5 s."
        );
        t.elapsed = Duration::from_secs(12);
        assert_eq!(
            t.outcome(),
            "Highest: Dining at 71%. Confidence 91%. Took 12 s."
        );
        t.top_rating = Some(("Gas".into(), 0.55));
        assert_eq!(
            t.outcome(),
            "Highest: Gas at 55%. Picked Dining at 91% confidence. Took 12 s."
        );
        t.top_rating = None;
        assert_eq!(t.outcome(), "Picked Dining at 91% confidence. Took 12 s.");
        t.category_name = None;
        t.guess = Some(CategoryGuess {
            category_id: None,
            confidence: 0.4,
        });
        t.top_rating = Some(("other".into(), 0.4));
        t.elapsed = Duration::from_millis(800);
        assert_eq!(
            t.outcome(),
            "Highest: other at 40%. Confidence 40%. Took 800 ms."
        );
    }

    #[test]
    fn highest_rating_reads_the_distribution() {
        let body = r#"{
  "answers": {
    "category": {
      "choice": "Dining",
      "probabilities": { "Dining": 0.40, "Gas": 0.55, "other": 0.05 },
      "confidence": 0.2
    }
  }
}"#;
        assert_eq!(highest_rating(body), Some(("Gas".into(), 0.55)));
        // A tie goes to the model's choice.
        let tied = r#"{"answers":{"category":{"choice":"Dining","probabilities":{"Dining":0.5,"Gas":0.5}}}}"#;
        assert_eq!(highest_rating(tied), Some(("Dining".into(), 0.5)));
        assert_eq!(
            highest_rating(r#"{"answers":{"category":{"choice":"Dining","probabilities":{}}}}"#),
            None
        );
        assert_eq!(highest_rating("not json"), None);
    }

    #[test]
    fn trace_errors_when_nothing_is_configured_or_row_is_missing() {
        let (_d, s, id) = store_with_row("ACME", -100);
        let t = ScriptedTransport::new(vec![]);
        assert!(trace_transaction(&s, &t, &id).is_err());
        configure(&s);
        s.add_category("Stuff").unwrap();
        assert!(trace_transaction(&s, &t, "nope").is_err());
    }

    #[test]
    fn pretty_body_handles_json_and_text() {
        assert_eq!(pretty_body(br#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty_body(b"plain"), "plain");
    }
}
