//! AI-assisted categorization. A `Categorizer` asks a remote model which category a transaction
//! belongs to; the pass in `run` applies the answer when its confidence clears the user's
//! threshold. typesafe.ai is the first provider; add another by implementing `Categorizer`
//! and listing it in `PROVIDERS`. Keys are never logged or included in error text.

mod debug;
mod lmr;
mod run;
mod systemone;
mod typesafe;

pub use debug::{trace_transaction, AiExchange, AiTrace, RecordingTransport};
pub use lmr::Lmr;
pub use run::{categorize_txns, categorize_uncategorized, run_after_sync, AiRunReport};
pub use typesafe::TypesafeAi;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::Error;
use crate::providers::Transport;

/// Default confidence needed before an AI answer is applied.
pub const DEFAULT_THRESHOLD: f64 = 0.70;

/// An API key. Debug prints `***`; the value is wiped when dropped.
#[derive(Clone, Default, Zeroize, ZeroizeOnDrop)]
pub struct AiSecret(pub String);

impl std::fmt::Debug for AiSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AiSecret(***)")
    }
}

impl AiSecret {
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

/// Provider-agnostic settings, stored in the encrypted ledger.
#[derive(Debug, Clone)]
pub struct AiSettings {
    /// `provider_id` of the chosen `Categorizer`, or `None` when nothing is set up.
    pub provider: Option<String>,
    pub api_key: AiSecret,
    /// Base URL of a self-hosted server, for providers with a `default_endpoint`. `None` means
    /// the provider's default.
    pub endpoint: Option<String>,
    /// Extra trusted root certificate (PEM) for that server, when it is self-signed.
    pub ca_cert: Option<String>,
    /// 0..1. Answers below this stay uncategorized.
    pub threshold: f64,
    /// Run the pass at the end of every sync.
    pub after_sync: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: None,
            api_key: AiSecret::default(),
            endpoint: None,
            ca_cert: None,
            threshold: DEFAULT_THRESHOLD,
            after_sync: false,
        }
    }
}

impl AiSettings {
    /// True when the chosen provider is known and either does not need a key or has one.
    pub fn is_configured(&self) -> bool {
        self.provider
            .as_deref()
            .and_then(categorizer_for)
            .is_some_and(|p| !p.needs_key() || !self.api_key.is_empty())
    }

    /// The saved server URL with surrounding whitespace and trailing slashes removed, or
    /// `default` when nothing usable is saved.
    pub fn endpoint_or<'a>(&'a self, default: &'a str) -> &'a str {
        match self
            .endpoint
            .as_deref()
            .map(|e| e.trim().trim_end_matches('/'))
        {
            Some(e) if !e.is_empty() => e,
            _ => default,
        }
    }

    /// The saved certificate, or `None` when the field is blank.
    pub fn ca_cert_pem(&self) -> Option<&str> {
        self.ca_cert
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }
}

/// One category the model may pick, with the user's description as context.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    /// Sign of the amount decides the direction; zero counts as money out.
    pub fn from_amount(amount_cents: i64) -> Self {
        if amount_cents > 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }
}

/// What the model sees about a transaction. Deliberately no amounts or account identifiers.
#[derive(Debug, Clone, PartialEq)]
pub struct CategorizeInput {
    pub title: String,
    pub direction: Direction,
}

/// The model's pick. `category_id: None` means it chose "other" (nothing fits).
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryGuess {
    pub category_id: Option<String>,
    /// 0..1, as reported by the provider.
    pub confidence: f64,
}

/// A remote model that picks one category for a transaction.
pub trait Categorizer: Send + Sync {
    /// Stable id stored in settings, e.g. `typesafe`.
    fn provider_id(&self) -> &'static str;
    /// Name shown in the provider dropdown.
    fn label(&self) -> &'static str;
    /// Where to get a key. Shown as a hint in Setup.
    fn key_help(&self) -> &'static str;
    /// Whether this provider needs a key to work. `true` unless overridden, e.g. a local model
    /// that only needs one when it was started with its own auth token.
    fn needs_key(&self) -> bool {
        true
    }
    /// One or two sentences on what the provider is, shown under the dropdown when selected.
    fn description(&self) -> &'static str;
    /// Optional caveat shown under the description, in gold, when this provider is selected.
    fn warning(&self) -> Option<&'static str> {
        None
    }
    /// A place to read more, as (link text, URL). Shown after the description.
    fn link(&self) -> Option<(&'static str, &'static str)> {
        None
    }
    /// For self-hosted providers: the base URL used when `AiSettings::endpoint` is unset.
    /// `Some` makes Setup show the server URL and certificate fields.
    fn default_endpoint(&self) -> Option<&'static str> {
        None
    }
    /// Ask the model. `settings` carries the key and, for self-hosted providers, the server
    /// URL and certificate; threshold and after-sync are not the provider's concern.
    fn categorize(
        &self,
        settings: &AiSettings,
        input: &CategorizeInput,
        options: &[CategoryOption],
        transport: &dyn Transport,
    ) -> Result<CategoryGuess, Error>;
}

/// Every provider the app knows. Adding one is one line here.
pub const PROVIDERS: &[&dyn Categorizer] = &[&TypesafeAi, &Lmr];

/// Previous stored id for LMR, from when the provider was named Laya.
pub const LEGACY_LMR_ID: &str = "laya-local";

/// Look a provider up by its stored id. `laya-local` still resolves to LMR.
pub fn categorizer_for(provider_id: &str) -> Option<&'static dyn Categorizer> {
    let provider_id = if provider_id == LEGACY_LMR_ID {
        "lmr"
    } else {
        provider_id
    };
    PROVIDERS
        .iter()
        .copied()
        .find(|p| p.provider_id() == provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let s = AiSecret("sk-live-abc".into());
        assert_eq!(format!("{s:?}"), "AiSecret(***)");
        let settings = AiSettings {
            api_key: s,
            ..Default::default()
        };
        assert!(!format!("{settings:?}").contains("abc"));
    }

    #[test]
    fn registry_finds_typesafe() {
        assert_eq!(categorizer_for("typesafe").unwrap().label(), "typesafe.ai");
        assert!(categorizer_for("nope").is_none());
    }

    #[test]
    fn registry_finds_lmr() {
        assert_eq!(categorizer_for("lmr").unwrap().label(), "LMR");
        assert_eq!(categorizer_for("laya-local").unwrap().provider_id(), "lmr");
    }

    #[test]
    fn direction_from_sign() {
        assert_eq!(Direction::from_amount(100), Direction::In);
        assert_eq!(Direction::from_amount(-100), Direction::Out);
        assert_eq!(Direction::from_amount(0), Direction::Out);
    }

    #[test]
    fn configured_needs_provider_and_key() {
        let mut s = AiSettings::default();
        assert!(!s.is_configured());
        s.provider = Some("typesafe".into());
        assert!(!s.is_configured());
        s.api_key = AiSecret("k".into());
        assert!(s.is_configured());
    }

    #[test]
    fn configured_ignores_missing_key_when_not_needed() {
        let s = AiSettings {
            provider: Some("lmr".into()),
            ..Default::default()
        };
        assert!(s.is_configured());
    }

    #[test]
    fn endpoint_falls_back_and_trims() {
        let mut s = AiSettings::default();
        assert_eq!(
            s.endpoint_or("http://127.0.0.1:8321"),
            "http://127.0.0.1:8321"
        );
        s.endpoint = Some("  https://laya.home:8321/  ".into());
        assert_eq!(s.endpoint_or("x"), "https://laya.home:8321");
        s.endpoint = Some("   ".into());
        assert_eq!(s.endpoint_or("x"), "x");
        assert!(s.ca_cert_pem().is_none());
        s.ca_cert = Some(" -----BEGIN CERTIFICATE----- ".into());
        assert_eq!(s.ca_cert_pem(), Some("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn every_provider_describes_itself() {
        for p in PROVIDERS {
            assert!(!p.description().is_empty(), "{}", p.provider_id());
        }
    }

    #[test]
    fn lmr_warns_about_categorization() {
        let lmr = categorizer_for("lmr").unwrap();
        let warning = lmr.warning().expect("lmr should warn");
        assert!(warning.contains("will not match typesafe.ai"), "{warning}");
        assert!(warning.contains("openbmb/MiniCPM5-2B-GGUF"), "{warning}");
        assert!(categorizer_for("typesafe").unwrap().warning().is_none());
    }

    #[test]
    fn configured_false_for_unknown_provider() {
        let s = AiSettings {
            provider: Some("nope".into()),
            api_key: AiSecret("k".into()),
            ..Default::default()
        };
        assert!(!s.is_configured());
    }
}
