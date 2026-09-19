//! AI-assisted categorization. A `Categorizer` asks a remote model which category a transaction
//! belongs to; the pass in `run` applies the answer when its confidence clears the user's
//! threshold. typesafe.ai is the first provider; add another by implementing `Categorizer`
//! and listing it in `PROVIDERS`. Keys are never logged or included in error text.

mod debug;
mod run;
mod typesafe;

pub use debug::{trace_transaction, AiExchange, AiTrace, RecordingTransport};
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
            threshold: DEFAULT_THRESHOLD,
            after_sync: false,
        }
    }
}

impl AiSettings {
    /// True when a provider and a key are both present.
    pub fn is_configured(&self) -> bool {
        self.provider.is_some() && !self.api_key.is_empty()
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
    fn categorize(
        &self,
        key: &AiSecret,
        input: &CategorizeInput,
        options: &[CategoryOption],
        transport: &dyn Transport,
    ) -> Result<CategoryGuess, Error>;
}

/// Every provider the app knows. Adding one is one line here.
pub const PROVIDERS: &[&dyn Categorizer] = &[&TypesafeAi];

/// Look a provider up by its stored id.
pub fn categorizer_for(provider_id: &str) -> Option<&'static dyn Categorizer> {
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
}
