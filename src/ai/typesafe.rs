//! typesafe.ai provider: one `choice` question per transaction against the System One model.
//! API: `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <key>`. The
//! wire protocol itself (request/response shapes, retries, error mapping) lives in
//! `ai::systemone`, shared with `ai::Lmr`.

use std::time::Duration;

use super::systemone::post_systemone;
use super::{AiSettings, CategorizeInput, Categorizer, CategoryGuess, CategoryOption};
use crate::error::Error;
use crate::providers::Transport;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// Matches the typesafe.ai SDK default of 60s per attempt.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct TypesafeAi;

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

    fn description(&self) -> &'static str {
        "Hosted System One model. Each uncategorized payee is sent to api.typesafe.ai with your category descriptions; billed per request by typesafe.ai."
    }

    fn link(&self) -> Option<(&'static str, &'static str)> {
        Some(("typesafe.ai", "https://typesafe.ai"))
    }

    /// One call per transaction. 429 and 529 are retried with backoff; every other failure is a
    /// user-facing error that never contains the key.
    fn categorize(
        &self,
        settings: &AiSettings,
        input: &CategorizeInput,
        options: &[CategoryOption],
        transport: &dyn Transport,
    ) -> Result<CategoryGuess, Error> {
        if settings.api_key.is_empty() {
            return Err(Error::user("Add an AI key in Setup → AI first."));
        }
        post_systemone(
            ENDPOINT,
            &settings.api_key,
            None,
            REQUEST_TIMEOUT,
            input,
            options,
            transport,
            "Network error talking to the AI service.",
            "The AI service timed out. Try again in a moment.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSecret, Direction};
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
        ]
    }

    fn input() -> CategorizeInput {
        CategorizeInput {
            title: "AMEX EPAYMENT ACH PMT".into(),
            direction: Direction::Out,
        }
    }

    fn with_key(key: impl Into<String>) -> AiSettings {
        AiSettings {
            provider: Some("typesafe".into()),
            api_key: AiSecret(key.into()),
            ..Default::default()
        }
    }

    fn ok_body(choice: &str, confidence: f64) -> Vec<u8> {
        format!(
            r#"{{"model":"jev-1","answers":{{"category":{{"type":"choice","choice":"{choice}","probabilities":{{}},"confidence":{confidence}}}}},"usage":{{}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn sends_bearer_and_json_body() {
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: ok_body("Gas", 0.9),
        }]);
        let g = TypesafeAi
            .categorize(&with_key(" sk-1 "), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-gas"));
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, ENDPOINT);
        assert_eq!(
            reqs[0].1,
            vec![("Authorization".to_string(), "Bearer sk-1".to_string())]
        );
        assert!(!reqs[0].2.is_empty());
        assert_eq!(reqs[0].4, Some(REQUEST_TIMEOUT));
    }

    #[test]
    fn errors_never_carry_the_key() {
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 401,
            body: b"{}".to_vec(),
        }]);
        let e = TypesafeAi
            .categorize(&with_key("sk-secret"), &input(), &opts(), &t)
            .unwrap_err();
        assert_eq!(e.as_user_message(), "AI key was rejected.");
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 422,
            body: br#"{"message":"bad <criteria> sk-secret"}"#.to_vec(),
        }]);
        let e = TypesafeAi
            .categorize(&with_key("sk-secret"), &input(), &opts(), &t)
            .unwrap_err();
        let msg = e.as_user_message();
        assert!(msg.starts_with("AI service rejected the request."));
        assert!(!msg.contains('<'));
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 500,
            body: vec![],
        }]);
        let e = TypesafeAi
            .categorize(&with_key("sk-secret"), &input(), &opts(), &t)
            .unwrap_err();
        assert_eq!(e.as_user_message(), "AI service returned HTTP 500.");
        let e = TypesafeAi
            .categorize(&with_key(""), &input(), &opts(), &t)
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
            .categorize(&with_key("k"), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-dining"));
        assert_eq!(t.calls(), 2);
    }
}
