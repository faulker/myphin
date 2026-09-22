//! Live check against a running `lmr-rs serve` (127.0.0.1:8321 by default; see `docs/ai.md`).
//! Run: `cargo test -- --ignored --nocapture live_lmr`

use myphin::ai::{AiSettings, CategorizeInput, Categorizer, CategoryOption, Direction, Lmr};
use myphin::providers::ReqwestTransport;

fn options() -> Vec<CategoryOption> {
    vec![
        CategoryOption {
            id: "c-dining".into(),
            name: "Dining".into(),
            description: Some("Restaurants, cafes, bars".into()),
        },
        CategoryOption {
            id: "c-cc".into(),
            name: "Credit Card Payment".into(),
            description: Some("Payments to a card issuer".into()),
        },
        CategoryOption {
            id: "c-transfer".into(),
            name: "Transfers".into(),
            description: Some("Moves between own accounts".into()),
        },
    ]
}

#[test]
#[ignore = "needs `lmr-rs serve` running locally"]
fn live_lmr_categorizes_over_loopback() {
    let transport = ReqwestTransport::new().unwrap();
    let input = CategorizeInput {
        title: "STARBUCKS STORE 1234".into(),
        direction: Direction::Out,
    };
    // Defaults: loopback URL, no key, no certificate. Set LMR_URL, LMR_KEY, and LMR_CERT
    // (path to the server's cert.pem) to try a self-hosted TLS server instead.
    let settings = AiSettings {
        provider: Some("lmr".into()),
        endpoint: std::env::var("LMR_URL").ok(),
        api_key: myphin::ai::AiSecret(std::env::var("LMR_KEY").unwrap_or_default()),
        ca_cert: std::env::var("LMR_CERT")
            .ok()
            .map(|p| std::fs::read_to_string(p).expect("LMR_CERT readable")),
        ..Default::default()
    };
    let guess = Lmr
        .categorize(&settings, &input, &options(), &transport)
        .expect("lmr-rs answered");
    println!("guess: {guess:?}");
    // The answer is the model's opinion; what this checks is that the loopback exchange
    // works without a key and maps back to one of our category ids (or "other").
    assert!((0.0..=1.0).contains(&guess.confidence));
    if let Some(id) = &guess.category_id {
        assert!(options().iter().any(|o| &o.id == id), "unknown id {id}");
    }
}
