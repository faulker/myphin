//! typesafe.ai provider: one `choice` question per transaction against the System One model.
//! API: `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <key>`.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::{AiSecret, CategorizeInput, Categorizer, CategoryGuess, CategoryOption, Direction};
use crate::error::{Error, InternalError};
use crate::providers::{Transport, TransportRequest};
use crate::sanitize::sanitize_user_text;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
const QUESTION_ID: &str = "category";
/// The choice key that means "nothing fits".
const OTHER: &str = "other";
const INSTRUCTIONS: &str =
    "Which spending category does this bank transaction belong to? Pick other if none fits.";
/// Retries on 429 / 529, with these waits in between.
const BACKOFF_SECS: &[u64] = &[1, 2, 4];

pub struct TypesafeAi;

#[derive(Serialize)]
struct State<'a> {
    #[serde(rename = "transactionTitle")]
    transaction_title: &'a str,
    direction: &'static str,
}

#[derive(Serialize)]
struct Question<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'static str,
    /// BTreeMap keeps the key order stable so request bodies are deterministic in tests.
    criteria: BTreeMap<String, Option<&'a str>>,
}

#[derive(Serialize)]
struct Request<'a> {
    state: State<'a>,
    model: &'static str,
    questions: BTreeMap<&'static str, Question<'a>>,
}

#[derive(Deserialize)]
struct Response {
    answers: HashMap<String, Answer>,
}

#[derive(Deserialize)]
struct Answer {
    choice: String,
    confidence: f64,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(alias = "message", alias = "detail", alias = "error")]
    message: Option<serde_json::Value>,
}

/// Build the JSON body and the map from criteria key back to category id. Keys are the
/// sanitized category names (the model reads them), made unique with a numeric suffix.
pub fn build_request(
    input: &CategorizeInput,
    options: &[CategoryOption],
) -> Result<(Vec<u8>, HashMap<String, String>), Error> {
    let mut criteria: BTreeMap<String, Option<&str>> = BTreeMap::new();
    let mut key_to_id = HashMap::new();
    for opt in options {
        let base = sanitize_user_text(opt.name.trim());
        if base.is_empty() || base.eq_ignore_ascii_case(OTHER) {
            continue;
        }
        let mut key = base.clone();
        let mut n = 2;
        while criteria.contains_key(&key) {
            key = format!("{base} ({n})");
            n += 1;
        }
        let desc = opt
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        criteria.insert(key.clone(), desc);
        key_to_id.insert(key, opt.id.clone());
    }
    if criteria.is_empty() {
        return Err(Error::user("Add at least one category before using AI."));
    }
    criteria.insert(OTHER.to_string(), None);
    let mut questions = BTreeMap::new();
    questions.insert(
        QUESTION_ID,
        Question {
            kind: "choice",
            instructions: INSTRUCTIONS,
            criteria,
        },
    );
    let req = Request {
        state: State {
            transaction_title: &input.title,
            direction: match input.direction {
                Direction::In => "money in",
                Direction::Out => "money out",
            },
        },
        model: MODEL,
        questions,
    };
    let body = serde_json::to_vec(&req).map_err(|_| Error::Internal(InternalError::Json))?;
    Ok((body, key_to_id))
}

/// Read the `category` answer out of a 200 response. Unknown or `other` choices map to `None`.
pub fn parse_response(
    body: &[u8],
    key_to_id: &HashMap<String, String>,
) -> Result<CategoryGuess, Error> {
    let parsed: Response =
        serde_json::from_slice(body).map_err(|_| Error::Internal(InternalError::Json))?;
    let answer = parsed
        .answers
        .get(QUESTION_ID)
        .ok_or(Error::Internal(InternalError::Json))?;
    Ok(CategoryGuess {
        category_id: key_to_id.get(&answer.choice).cloned(),
        confidence: answer.confidence.clamp(0.0, 1.0),
    })
}

/// Short, sanitized message from an error body, if it has one.
fn error_message(body: &[u8]) -> Option<String> {
    let parsed: ErrorBody = serde_json::from_slice(body).ok()?;
    let msg = match parsed.message? {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    };
    let msg = sanitize_user_text(&msg);
    if msg.is_empty() {
        None
    } else {
        Some(msg.chars().take(160).collect())
    }
}

impl Categorizer for TypesafeAi {
    fn provider_id(&self) -> &'static str {
        "typesafe"
    }

    fn label(&self) -> &'static str {
        "typesafe.ai"
    }

    fn key_help(&self) -> &'static str {
        "API key from typesafe.ai (docs.typesafe.ai)"
    }

    /// One call per transaction. 429 and 529 are retried with backoff; every other failure is a
    /// user-facing error that never contains the key.
    fn categorize(
        &self,
        key: &AiSecret,
        input: &CategorizeInput,
        options: &[CategoryOption],
        transport: &dyn Transport,
    ) -> Result<CategoryGuess, Error> {
        if key.is_empty() {
            return Err(Error::user("Add an AI key in Setup → AI first."));
        }
        let (body, key_to_id) = build_request(input, options)?;
        let req = TransportRequest {
            method: "POST",
            url: ENDPOINT.to_string(),
            headers: vec![("Authorization".into(), format!("Bearer {}", key.0.trim()))],
            body: Some(body),
            ..Default::default()
        };
        let mut attempt = 0;
        loop {
            let resp = transport
                .send(&req)
                .map_err(|_| Error::user("Network error talking to the AI service."))?;
            match resp.status {
                200..=299 => return parse_response(&resp.body, &key_to_id),
                401 | 403 => return Err(Error::user("AI key was rejected.")),
                422 => {
                    let detail = error_message(&resp.body)
                        .map(|m| format!(" {m}"))
                        .unwrap_or_default();
                    return Err(Error::user(format!(
                        "AI service rejected the request.{detail}"
                    )));
                }
                429 | 529 => {
                    if attempt >= BACKOFF_SECS.len() {
                        return Err(Error::user("AI service is busy, try again later."));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(BACKOFF_SECS[attempt]));
                    attempt += 1;
                }
                status => {
                    return Err(Error::user(format!("AI service returned HTTP {status}.")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ScriptedTransport, TransportResponse};

    fn opts() -> Vec<CategoryOption> {
        vec![
            CategoryOption {
                id: "c-dining".into(),
                name: "Dining".into(),
                description: Some("Restaurants, cafes, bars".into()),
            },
            CategoryOption {
                id: "c-gas".into(),
                name: "Gas".into(),
                description: None,
            },
            CategoryOption {
                id: "c-gas2".into(),
                name: "Gas".into(),
                description: Some("  ".into()),
            },
            CategoryOption {
                id: "c-other".into(),
                name: "Other".into(),
                description: None,
            },
        ]
    }

    fn input() -> CategorizeInput {
        CategorizeInput {
            title: "AMEX EPAYMENT ACH PMT".into(),
            direction: Direction::Out,
        }
    }

    fn ok_body(choice: &str, confidence: f64) -> Vec<u8> {
        format!(
            r#"{{"model":"jev-1","answers":{{"category":{{"type":"choice","choice":"{choice}","probabilities":{{}},"confidence":{confidence}}}}},"usage":{{}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn request_matches_typesafe_shape() {
        let (body, map) = build_request(&input(), &opts()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "jev-latest");
        assert_eq!(v["state"]["transactionTitle"], "AMEX EPAYMENT ACH PMT");
        assert_eq!(v["state"]["direction"], "money out");
        let q = &v["questions"]["category"];
        assert_eq!(q["type"], "choice");
        assert_eq!(q["instructions"], INSTRUCTIONS);
        assert_eq!(q["criteria"]["Dining"], "Restaurants, cafes, bars");
        assert!(q["criteria"]["Gas"].is_null());
        assert!(q["criteria"]["Gas (2)"].is_null());
        assert!(q["criteria"]["other"].is_null());
        // A user category literally named "Other" cannot shadow the built-in escape hatch.
        assert!(q["criteria"].get("Other").is_none());
        assert_eq!(map["Dining"], "c-dining");
        assert_eq!(map["Gas (2)"], "c-gas2");
        assert!(!map.contains_key("other"));
    }

    #[test]
    fn request_needs_a_category() {
        assert!(build_request(&input(), &[]).is_err());
    }

    #[test]
    fn response_maps_choice_and_other() {
        let (_, map) = build_request(&input(), &opts()).unwrap();
        let g = parse_response(&ok_body("Dining", 0.82), &map).unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-dining"));
        assert!((g.confidence - 0.82).abs() < 1e-9);
        let g = parse_response(&ok_body("other", 0.4), &map).unwrap();
        assert_eq!(g.category_id, None);
        let g = parse_response(&ok_body("Unknown", 0.9), &map).unwrap();
        assert_eq!(g.category_id, None);
        assert!(parse_response(b"not json", &map).is_err());
    }

    #[test]
    fn sends_bearer_and_json_body() {
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: ok_body("Gas", 0.9),
        }]);
        let g = TypesafeAi
            .categorize(&AiSecret(" sk-1 ".into()), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-gas"));
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, ENDPOINT);
        assert_eq!(
            reqs[0].1,
            vec![("Authorization".to_string(), "Bearer sk-1".to_string())]
        );
        assert!(!reqs[0].2.is_empty());
    }

    #[test]
    fn errors_never_carry_the_key() {
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 401,
            body: b"{}".to_vec(),
        }]);
        let e = TypesafeAi
            .categorize(&AiSecret("sk-secret".into()), &input(), &opts(), &t)
            .unwrap_err();
        assert_eq!(e.as_user_message(), "AI key was rejected.");
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 422,
            body: br#"{"message":"bad <criteria> sk-secret"}"#.to_vec(),
        }]);
        let e = TypesafeAi
            .categorize(&AiSecret("sk-secret".into()), &input(), &opts(), &t)
            .unwrap_err();
        let msg = e.as_user_message();
        assert!(msg.starts_with("AI service rejected the request."));
        assert!(!msg.contains('<'));
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 500,
            body: vec![],
        }]);
        let e = TypesafeAi
            .categorize(&AiSecret("sk-secret".into()), &input(), &opts(), &t)
            .unwrap_err();
        assert_eq!(e.as_user_message(), "AI service returned HTTP 500.");
        let e = TypesafeAi
            .categorize(&AiSecret(String::new()), &input(), &opts(), &t)
            .unwrap_err();
        assert!(e.as_user_message().contains("Setup"));
    }

    #[test]
    fn retries_after_busy() {
        let t = ScriptedTransport::new(vec![
            TransportResponse {
                status: 429,
                body: vec![],
            },
            TransportResponse {
                status: 200,
                body: ok_body("Dining", 0.75),
            },
        ]);
        let g = TypesafeAi
            .categorize(&AiSecret("k".into()), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-dining"));
        assert_eq!(t.calls(), 2);
    }
}
