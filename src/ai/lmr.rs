//! Self-hosted model provider: talks to an `lmr-rs` server (<https://github.com/faulker/lmr-rs>),
//! which serves the same System One wire protocol as typesafe.ai (shared through
//! `ai::systemone`). Best local results come from `openbmb/MiniCPM5-2B-GGUF`; they still trail
//! typesafe.ai. The server is `http://127.0.0.1:8321` by default; Setup can point it at another
//! machine, with an optional API key and, for a self-signed server certificate, the PEM to trust.

use std::time::Duration;

use super::systemone::post_systemone;
use super::{AiSettings, CategorizeInput, Categorizer, CategoryGuess, CategoryOption};
use crate::error::Error;
use crate::providers::{url_allowed, Transport};

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8321";
const PATH: &str = "/v1/systemone";
/// CPU inference is ~10s per forward on an M-series Mac; a tournament over a large category
/// list is two or three forwards, plus warmup. reqwest's blocking client otherwise cuts the
/// request off at 30s and the user sees a "can't reach the server" error.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub struct Lmr;

impl Categorizer for Lmr {
    fn provider_id(&self) -> &'static str {
        "lmr"
    }

    fn label(&self) -> &'static str {
        "LMR"
    }

    fn key_help(&self) -> &'static str {
        "Optional. Only needed if the server has an API key set (server.api_key in its config)."
    }

    fn needs_key(&self) -> bool {
        false
    }

    fn description(&self) -> &'static str {
        "A model you run yourself with lmr-rs, on this machine or one on your network. Nothing is sent to a third party and there is no per-request cost."
    }

    fn warning(&self) -> Option<&'static str> {
        Some(
            "A model you host will not match typesafe.ai. For the best local results, serve openbmb/MiniCPM5-2B-GGUF.",
        )
    }

    fn link(&self) -> Option<(&'static str, &'static str)> {
        Some((
            "github.com/faulker/lmr-rs",
            "https://github.com/faulker/lmr-rs",
        ))
    }

    fn default_endpoint(&self) -> Option<&'static str> {
        Some(DEFAULT_ENDPOINT)
    }

    /// One call per transaction, same retry/error handling as typesafe.ai. The bearer header is
    /// sent only when a key was set, and the saved certificate (if any) is trusted for this
    /// server only.
    fn categorize(
        &self,
        settings: &AiSettings,
        input: &CategorizeInput,
        options: &[CategoryOption],
        transport: &dyn Transport,
    ) -> Result<CategoryGuess, Error> {
        let base = settings.endpoint_or(DEFAULT_ENDPOINT);
        if !url_allowed(base) {
            return Err(Error::user(
                "LMR server URL must be https, or http on a local network address.",
            ));
        }
        let endpoint = format!("{base}{PATH}");
        post_systemone(
            &endpoint,
            &settings.api_key,
            settings.ca_cert_pem(),
            REQUEST_TIMEOUT,
            input,
            options,
            transport,
            "Can't reach the model server. Check that lmr-rs is running at the URL in Setup → AI, and paste its certificate there if it uses a self-signed https cert.",
            "The model server timed out. CPU inference can take a minute; check that lmr-rs is running.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSecret, Direction};
    use crate::providers::{ScriptedTransport, TransportResponse};

    fn opts() -> Vec<CategoryOption> {
        vec![CategoryOption {
            id: "c-dining".into(),
            name: "Dining".into(),
            description: None,
        }]
    }

    fn input() -> CategorizeInput {
        CategorizeInput {
            title: "COFFEE SHOP".into(),
            direction: Direction::Out,
        }
    }

    fn settings() -> AiSettings {
        AiSettings {
            provider: Some("lmr".into()),
            ..Default::default()
        }
    }

    fn ok_body(choice: &str, confidence: f64) -> Vec<u8> {
        format!(
            r#"{{"model":"lmr-rs","answers":{{"category":{{"type":"choice","choice":"{choice}","probabilities":{{}},"confidence":{confidence}}}}},"usage":{{}}}}"#
        )
        .into_bytes()
    }

    fn ok_transport() -> ScriptedTransport {
        ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: ok_body("Dining", 0.9),
        }])
    }

    #[test]
    fn defaults_to_loopback_with_no_auth_header_or_cert() {
        let t = ok_transport();
        let g = Lmr.categorize(&settings(), &input(), &opts(), &t).unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-dining"));
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "http://127.0.0.1:8321/v1/systemone");
        assert!(reqs[0].1.is_empty());
        assert!(reqs[0].3.is_none());
        assert_eq!(reqs[0].4, Some(REQUEST_TIMEOUT));
    }

    #[test]
    fn uses_saved_server_url_key_and_certificate() {
        let t = ok_transport();
        let s = AiSettings {
            api_key: AiSecret("tok".into()),
            endpoint: Some("https://laya.home:8321/".into()),
            ca_cert: Some("-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----\n".into()),
            ..settings()
        };
        Lmr.categorize(&s, &input(), &opts(), &t).unwrap();
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "https://laya.home:8321/v1/systemone");
        assert_eq!(
            reqs[0].1,
            vec![("Authorization".to_string(), "Bearer tok".to_string())]
        );
        assert!(reqs[0]
            .3
            .as_deref()
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn private_http_is_allowed_public_http_is_not() {
        let t = ok_transport();
        let s = AiSettings {
            endpoint: Some("http://192.168.1.20:8321".into()),
            ..settings()
        };
        Lmr.categorize(&s, &input(), &opts(), &t).unwrap();
        assert_eq!(
            t.requests.lock().unwrap()[0].0,
            "http://192.168.1.20:8321/v1/systemone"
        );

        let t = ok_transport();
        let s = AiSettings {
            endpoint: Some("http://laya.example.com".into()),
            ..settings()
        };
        let e = Lmr.categorize(&s, &input(), &opts(), &t).unwrap_err();
        assert_eq!(
            e.as_user_message(),
            "LMR server URL must be https, or http on a local network address."
        );
        assert_eq!(t.calls(), 0);
    }

    #[test]
    fn unreachable_server_points_at_setup() {
        let t = ScriptedTransport::new(vec![]);
        let e = Lmr
            .categorize(&settings(), &input(), &opts(), &t)
            .unwrap_err();
        assert_eq!(
            e.as_user_message(),
            "Can't reach the model server. Check that lmr-rs is running at the URL in Setup → AI, and paste its certificate there if it uses a self-signed https cert."
        );
    }

    #[test]
    fn timeout_is_not_reported_as_unreachable() {
        struct TimeoutTransport;
        impl crate::providers::Transport for TimeoutTransport {
            fn send(
                &self,
                _: &crate::providers::TransportRequest,
            ) -> Result<crate::providers::TransportResponse, crate::error::Error> {
                Err(crate::error::Error::user(
                    crate::providers::TRANSPORT_TIMEOUT_MSG,
                ))
            }
        }
        let e = Lmr
            .categorize(&settings(), &input(), &opts(), &TimeoutTransport)
            .unwrap_err();
        assert_eq!(
            e.as_user_message(),
            "The model server timed out. CPU inference can take a minute; check that lmr-rs is running."
        );
    }

    #[test]
    fn other_maps_to_none() {
        let t = ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: ok_body("other", 0.5),
        }]);
        let g = Lmr.categorize(&settings(), &input(), &opts(), &t).unwrap();
        assert_eq!(g.category_id, None);
    }
}
