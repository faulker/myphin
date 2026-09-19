//! Live check against a SimpleFIN Bridge demo token. Needs network.
//! Run: `cargo test -- --ignored --nocapture live_simplefin`

use myphin::providers::{ReqwestTransport, SimpleFinSource, TransactionSource};
use myphin::store::Store;
use myphin::sync::sync_connection;

fn fetch_demo_token() -> String {
    let html = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .build()
        .unwrap()
        .get("https://beta-bridge.simplefin.org/info/developers")
        .send()
        .unwrap()
        .text()
        .unwrap();
    for raw in html.split_whitespace() {
        let t = raw.trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == '<' || c == '>');
        if t.len() < 40 {
            continue;
        }
        if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, t) {
            if let Ok(url) = String::from_utf8(bytes) {
                if url.starts_with("https://")
                    && url.contains("/claim/")
                    && url.contains("simplefin")
                {
                    return t.to_string();
                }
            }
        }
    }
    panic!("no demo setup token found on the developer page");
}

#[test]
#[ignore]
fn live_simplefin_claim_fetch_dedup() {
    let token = fetch_demo_token();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "live-test-pass").unwrap();
    let transport = ReqwestTransport::new().unwrap();
    let source = SimpleFinSource;
    let secrets = source.claim(&token, &transport).expect("claim demo token");
    assert!(
        secrets.inner.starts_with("https://"),
        "access url should be https"
    );
    assert!(
        !format!("{secrets:?}").contains("://") || format!("{secrets:?}").contains("***"),
        "debug must not leak credentials: {secrets:?}"
    );
    let id = store.add_connection("simplefin", "Demo", &secrets).unwrap();
    let now = chrono::Utc::now().timestamp();
    let start = now - 30 * 86400;
    let a = sync_connection(&store, &id, &source, &transport, start, now).unwrap();
    let b = sync_connection(&store, &id, &source, &transport, start, now).unwrap();
    let n = store.list_transactions(false, None).unwrap().len();
    eprintln!("first={:?} second={:?} n={n}", a.stats, b.stats);
    assert!(
        n == 0 || b.stats.inserted == 0,
        "second sync inserted duplicates: first={:?} second={:?} n={n}",
        a.stats,
        b.stats
    );
}
