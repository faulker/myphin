//! Myphin library: sync, store, and budget domain. The binary is a Dioxus shell around this.

pub mod ai;
pub mod crypto;
pub mod domain;
pub mod error;
pub mod money;
pub mod providers;
pub mod redact;
pub mod sanitize;
pub mod store;
pub mod sync;

pub use domain::{
    current_year_month, month_bounds, split_patterns, Account, PaymentKind, Rule, RuleAction,
    TxnPatch, TxnQuery,
};
pub use error::Error;
pub use store::Store;
