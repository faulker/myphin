//! Any server that speaks the System One API (`POST <base>/v1/systemone`): Ollaya
//! (<https://ollaya.dev>) running `winnow:e4b` or `laya:e4b`, lmr-rs, typesafe.ai itself with
//! `jev-latest`, or a proxy in front of one of them. The user sets the base URL, the model name,
//! and optionally a key and a certificate to trust. The wire protocol is `ai::systemone`.

use std::time::Duration;

use super::systemone::post_systemone;
use super::{AiSettings, CategorizeInput, Categorizer, CategoryGuess, CategoryOption};
use crate::error::Error;
use crate::providers::{url_allowed, Transport};

/// Ollaya's default listen address.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11435";
/// Ollaya's example model. Typesafe.ai users set `jev-latest` instead.
pub const DEFAULT_MODEL: &str = "winnow:e4b";
const PATH: &str = "/v1/systemone";
/// The server may be a CPU-only local model, so allow as long as `ai::Lmr` does.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub struct SystemOne;

impl Categorizer for SystemOne {
    fn provider_id(&self) -> &'static str {
        "systemone"
    }

    fn label(&self) -> &'static str {
        "Any System One API"
    }

    fn key_help(&self) -> &'static str {
        "Optional. Needed for typesafe.ai, or a server with a key set (OLLAYA_API_KEY for Ollaya)."
    }

    fn needs_key(&self) -> bool {
        false
    }

    fn description(&self) -> &'static str {
        "Any server with a /v1/systemone endpoint: Ollaya on this machine or your network, lmr-rs, or typesafe.ai with model jev-latest. Set the server URL and the model it should run."
    }

    fn warning(&self) -> Option<&'static str> {
        Some(
            "A model you host will not match typesafe.ai's Jev. For Ollaya, laya:e4b is the closest for picking a category.",
        )
    }

    fn link(&self) -> Option<(&'static str, &'static str)> {
        Some(("ollaya.dev", "https://ollaya.dev"))
    }

    fn default_endpoint(&self) -> Option<&'static str> {
        Some(DEFAULT_ENDPOINT)
    }

    fn default_model(&self) -> Option<&'static str> {
        Some(DEFAULT_MODEL)
    }

    /// One call per transaction to `<base>/v1/systemone` with the saved model name. The bearer
    /// header is sent only when a key was set, and the saved certificate (if any) is trusted for
    /// this server only.
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
                "Server URL must be https, or http on a local network address.",
            ));
        }
        // Accept a URL pasted with the path already on it.
        let base = base.strip_suffix(PATH).unwrap_or(base);
        let endpoint = format!("{base}{PATH}");
        post_systemone(
            &endpoint,
            settings.model_or(DEFAULT_MODEL),
            &settings.api_key,
            settings.ca_cert_pem(),
            REQUEST_TIMEOUT,
            input,
            options,
            transport,
            "Can't reach the System One server. Check the URL in Setup → AI, that the server is running, and paste its certificate there if it uses a self-signed https cert.",
            "The System One server timed out. A local model on CPU can take a minute; check that it is running.",
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
            provider: Some("systemone".into()),
            ..Default::default()
        }
    }

    fn ok_transport(choice: &str) -> ScriptedTransport {
        ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: format!(
                r#"{{"model":"winnow:e4b","answers":{{"category":{{"type":"choice","choice":"{choice}","probabilities":{{}},"confidence":0.8123}}}},"usage":{{}}}}"#
            )
            .into_bytes(),
        }])
    }

    fn sent_model(t: &ScriptedTransport) -> String {
        let reqs = t.requests.lock().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&reqs[0].2).unwrap();
        v["model"].as_str().unwrap().to_string()
    }

    #[test]
    fn defaults_to_local_ollaya_with_default_model() {
        let t = ok_transport("Dining");
        let g = SystemOne
            .categorize(&settings(), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id.as_deref(), Some("c-dining"));
        assert!((g.confidence - 0.8123).abs() < 1e-9);
        assert_eq!(sent_model(&t), DEFAULT_MODEL);
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "http://127.0.0.1:11435/v1/systemone");
        assert!(reqs[0].1.is_empty());
        assert!(reqs[0].3.is_none());
        assert_eq!(reqs[0].4, Some(REQUEST_TIMEOUT));
    }

    #[test]
    fn typesafe_with_jev_model_and_key() {
        let t = ok_transport("Dining");
        let s = AiSettings {
            api_key: AiSecret("sk-1".into()),
            endpoint: Some("https://api.typesafe.ai".into()),
            model: Some("jev-latest".into()),
            ..settings()
        };
        SystemOne.categorize(&s, &input(), &opts(), &t).unwrap();
        assert_eq!(sent_model(&t), "jev-latest");
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "https://api.typesafe.ai/v1/systemone");
        assert_eq!(
            reqs[0].1,
            vec![("Authorization".to_string(), "Bearer sk-1".to_string())]
        );
    }

    #[test]
    fn url_with_path_already_on_is_not_doubled() {
        let t = ok_transport("Dining");
        let s = AiSettings {
            endpoint: Some("http://ollaya.local:11435/v1/systemone/".into()),
            model: Some("laya:e4b".into()),
            ..settings()
        };
        SystemOne.categorize(&s, &input(), &opts(), &t).unwrap();
        assert_eq!(
            t.requests.lock().unwrap()[0].0,
            "http://ollaya.local:11435/v1/systemone"
        );
        assert_eq!(sent_model(&t), "laya:e4b");
    }

    #[test]
    fn public_http_is_refused_without_a_call() {
        let t = ok_transport("Dining");
        let s = AiSettings {
            endpoint: Some("http://ollaya.example.com".into()),
            ..settings()
        };
        let e = SystemOne.categorize(&s, &input(), &opts(), &t).unwrap_err();
        assert_eq!(
            e.as_user_message(),
            "Server URL must be https, or http on a local network address."
        );
        assert_eq!(t.calls(), 0);
    }

    #[test]
    fn unreachable_server_points_at_setup() {
        let t = ScriptedTransport::new(vec![]);
        let e = SystemOne
            .categorize(&settings(), &input(), &opts(), &t)
            .unwrap_err();
        assert!(e.as_user_message().contains("Setup → AI"));
    }

    #[test]
    fn other_maps_to_none() {
        let t = ok_transport("other");
        let g = SystemOne
            .categorize(&settings(), &input(), &opts(), &t)
            .unwrap();
        assert_eq!(g.category_id, None);
    }
}
