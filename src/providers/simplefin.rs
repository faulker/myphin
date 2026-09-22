//! SimpleFIN Bridge / protocol v2 client.

use std::collections::HashMap;

use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::Error;
use crate::money::parse_cents;
use crate::providers::{
    AccountSet, ConnectionSecrets, DateWindow, NormalizedAccount, NormalizedTxn, SourceErrorItem,
    TransactionSource, Transport, TransportRequest,
};
use crate::sanitize::sanitize_user_text;

pub struct SimpleFinSource;

impl TransactionSource for SimpleFinSource {
    fn source_id(&self) -> &'static str {
        "simplefin"
    }

    fn claim(
        &self,
        setup_token: &str,
        transport: &dyn Transport,
    ) -> Result<ConnectionSecrets, Error> {
        let claim_url = decode_setup_token(setup_token)?;
        require_https(&claim_url)?;
        let resp = transport.send(&TransportRequest {
            method: "POST",
            url: claim_url,
            ..Default::default()
        })?;
        if resp.status == 403 {
            return Err(Error::user(
                "This setup token was already used or may be compromised. Disable it on SimpleFIN Bridge and create a new one.",
            ));
        }
        if resp.status != 200 {
            return Err(Error::user(format!(
                "Could not claim SimpleFIN token (HTTP {}).",
                resp.status
            )));
        }
        let access = String::from_utf8(resp.body)
            .map_err(|_| Error::user("SimpleFIN returned a non-text Access URL."))?
            .trim()
            .to_string();
        if !access.starts_with("https://") {
            return Err(Error::user(
                "SimpleFIN did not return an Access URL. The setup token may already be used.",
            ));
        }
        Ok(ConnectionSecrets { inner: access })
    }

    fn fetch(
        &self,
        secrets: &ConnectionSecrets,
        window: DateWindow,
        include_pending: bool,
        transport: &dyn Transport,
    ) -> Result<AccountSet, Error> {
        let parsed = Url::parse(&secrets.inner)
            .map_err(|_| Error::user("Stored SimpleFIN access is not a valid URL."))?;
        require_https(parsed.as_str())?;
        let user = parsed.username().to_string();
        let pass = parsed.password().unwrap_or("").to_string();
        let mut accounts_url = parsed.clone();
        let _ = accounts_url.set_username("");
        let _ = accounts_url.set_password(None);
        let mut path = accounts_url.path().trim_end_matches('/').to_string();
        path.push_str("/accounts");
        accounts_url.set_path(&path);

        let mut query = vec![
            ("version".into(), "2".into()),
            ("start-date".into(), window.start_date.to_string()),
            ("end-date".into(), window.end_date.to_string()),
        ];
        if include_pending {
            query.push(("pending".into(), "1".into()));
        }

        let resp = transport.send(&TransportRequest {
            method: "GET",
            url: accounts_url.to_string(),
            basic_user: Some(user),
            basic_pass: Some(pass),
            query,
            follow_redirects: true,
            ..Default::default()
        })?;
        match resp.status {
            200 => parse_account_set(&resp.body),
            402 => Err(Error::user(
                "SimpleFIN requires payment. Check your Bridge subscription.",
            )),
            403 => Err(Error::user(
                "SimpleFIN access was revoked or the credentials are wrong. Paste a new setup token in Setup.",
            )),
            other => Err(Error::user(format!(
                "SimpleFIN accounts request failed (HTTP {other})."
            ))),
        }
    }
}

fn decode_setup_token(token: &str) -> Result<String, Error> {
    let trimmed = token.trim();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .map_err(|_| Error::user("Setup token is not valid Base64."))?;
    String::from_utf8(decoded).map_err(|_| Error::user("Setup token is not a URL."))
}

fn require_https(url: &str) -> Result<(), Error> {
    let parsed = Url::parse(url).map_err(|_| Error::user("Not a valid URL."))?;
    if parsed.scheme() != "https" {
        return Err(Error::user("Refusing non-HTTPS SimpleFIN URL."));
    }
    Ok(())
}

#[derive(Deserialize)]
struct RawSet {
    #[serde(default)]
    errlist: Vec<RawErr>,
    /// Accounts stay as JSON values until parsed so the original object can be kept verbatim.
    #[serde(default)]
    accounts: Vec<Value>,
    #[serde(default)]
    connections: Vec<RawConnection>,
}

/// One institution login. The bank's name lives here, not on the account.
#[derive(Deserialize)]
struct RawConnection {
    conn_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
}

#[derive(Deserialize)]
struct RawErr {
    #[serde(default)]
    code: String,
    #[serde(default)]
    msg: String,
    conn_id: Option<String>,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct RawAccount {
    id: String,
    name: String,
    #[serde(default)]
    conn_id: String,
    currency: String,
    balance: String,
    #[serde(rename = "available-balance")]
    available_balance: Option<String>,
    #[serde(rename = "balance-date")]
    balance_date: i64,
    #[serde(default)]
    transactions: Vec<Value>,
}

#[derive(Deserialize)]
struct RawTxn {
    id: String,
    posted: i64,
    amount: String,
    description: String,
    transacted_at: Option<i64>,
    #[serde(default)]
    pending: bool,
}

fn parse_account_set(body: &[u8]) -> Result<AccountSet, Error> {
    let raw: RawSet = serde_json::from_slice(body)
        .map_err(|_| Error::user("SimpleFIN returned invalid JSON."))?;
    let errors = raw
        .errlist
        .into_iter()
        .map(|e| SourceErrorItem {
            code: e.code,
            message: sanitize_user_text(&e.msg),
            conn_id: e.conn_id,
            account_id: e.account_id,
        })
        .collect();
    let institutions: HashMap<String, String> = raw
        .connections
        .into_iter()
        .filter_map(|c| institution_name(c.org_name, c.name).map(|n| (c.conn_id, n)))
        .collect();
    let mut accounts = Vec::new();
    for value in raw.accounts {
        let a: RawAccount = serde_json::from_value(value.clone())
            .map_err(|_| Error::user("SimpleFIN returned invalid JSON."))?;
        let mut txns = Vec::new();
        for tv in a.transactions {
            let t: RawTxn = serde_json::from_value(tv.clone())
                .map_err(|_| Error::user("SimpleFIN returned invalid JSON."))?;
            txns.push(NormalizedTxn {
                remote_id: t.id,
                posted: t.posted,
                transacted_at: t.transacted_at,
                amount_cents: parse_cents(&t.amount)?,
                description: t.description,
                pending: t.pending,
                raw: Some(tv.to_string()),
            });
        }
        accounts.push(NormalizedAccount {
            remote_id: a.id,
            institution: institutions.get(&a.conn_id).cloned(),
            conn_id: a.conn_id,
            name: a.name,
            currency: a.currency,
            balance_cents: parse_cents(&a.balance)?,
            available_cents: a
                .available_balance
                .as_deref()
                .map(parse_cents)
                .transpose()?,
            balance_date: a.balance_date,
            transactions: txns,
            raw: Some(account_without_transactions(value)),
        });
    }
    Ok(AccountSet { errors, accounts })
}

/// The bank's display name from a connection: its `org_name`, or the login's `name` when the
/// bridge sent no organisation. Blank strings count as missing.
fn institution_name(org_name: Option<String>, name: Option<String>) -> Option<String> {
    [org_name, name]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
}

/// The account object as sent, with its `transactions` array dropped: each transaction keeps
/// its own raw copy, so repeating them here would only bloat the ledger.
fn account_without_transactions(mut value: Value) -> String {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("transactions");
    }
    value.to_string()
}

/// In-memory transport for tests: one canned response per URL (exact, then prefix match).
pub struct MapTransport {
    pub by_url: HashMap<String, crate::providers::TransportResponse>,
    pub last_urls: std::sync::Mutex<Vec<String>>,
    pub last_bodies: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl MapTransport {
    pub fn new() -> Self {
        Self {
            by_url: HashMap::new(),
            last_urls: std::sync::Mutex::new(Vec::new()),
            last_bodies: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Transport for MapTransport {
    fn send(&self, req: &TransportRequest) -> Result<crate::providers::TransportResponse, Error> {
        if !crate::providers::url_allowed(&req.url) {
            return Err(Error::user("Refusing non-HTTPS URL."));
        }
        self.last_urls.lock().unwrap().push(req.url.clone());
        self.last_bodies
            .lock()
            .unwrap()
            .push(req.body.clone().unwrap_or_default());
        self.by_url
            .get(&req.url)
            .cloned()
            .or_else(|| {
                self.by_url.iter().find_map(|(k, v)| {
                    if req.url.starts_with(k) {
                        Some(v.clone())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| Error::user("mock miss"))
    }
}

/// In-memory transport for tests that answers calls in order, whatever the URL, and records
/// every request's headers and body so tests can assert on what was sent.
pub struct ScriptedTransport {
    pub responses:
        std::sync::Mutex<std::collections::VecDeque<crate::providers::TransportResponse>>,
    pub requests: std::sync::Mutex<Vec<RecordedRequest>>,
}

/// What a `ScriptedTransport` saw: (url, headers, body, extra root certificate, timeout).
pub type RecordedRequest = (
    String,
    Vec<(String, String)>,
    Vec<u8>,
    Option<String>,
    Option<std::time::Duration>,
);

impl ScriptedTransport {
    pub fn new(responses: Vec<crate::providers::TransportResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// How many calls were made.
    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Transport for ScriptedTransport {
    fn send(&self, req: &TransportRequest) -> Result<crate::providers::TransportResponse, Error> {
        if !crate::providers::url_allowed(&req.url) {
            return Err(Error::user("Refusing non-HTTPS URL."));
        }
        self.requests.lock().unwrap().push((
            req.url.clone(),
            req.headers.clone(),
            req.body.clone().unwrap_or_default(),
            req.root_cert_pem.clone(),
            req.timeout,
        ));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::user("mock miss"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{TransactionSource, TransportResponse};

    #[test]
    fn transports_record_headers_and_bodies() {
        let req = TransportRequest {
            method: "POST",
            url: "https://api.example.com/x".into(),
            headers: vec![("Authorization".into(), "Bearer k".into())],
            body: Some(b"{}".to_vec()),
            ..Default::default()
        };
        assert!(req.basic_user.is_none() && req.query.is_empty() && !req.follow_redirects);

        let mut m = MapTransport::new();
        m.by_url.insert(
            "https://api.example.com/".into(),
            TransportResponse {
                status: 200,
                body: vec![],
            },
        );
        m.send(&req).unwrap();
        assert_eq!(m.last_bodies.lock().unwrap()[0], b"{}".to_vec());

        let s = ScriptedTransport::new(vec![TransportResponse {
            status: 200,
            body: b"a".to_vec(),
        }]);
        assert_eq!(s.send(&req).unwrap().body, b"a".to_vec());
        assert!(s.send(&req).is_err());
        assert_eq!(s.calls(), 2);
        assert_eq!(s.requests.lock().unwrap()[0].1[0].0, "Authorization");
        // Public plain HTTP is refused before anything is recorded.
        assert!(s
            .send(&TransportRequest {
                url: "http://example.com".into(),
                ..Default::default()
            })
            .is_err());
        assert_eq!(s.calls(), 2);
    }

    fn b64(url: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(url.as_bytes())
    }

    #[test]
    fn claim_rejects_http() {
        let token = b64("http://evil.example/claim/x");
        let t = MapTransport::new();
        let err = SimpleFinSource.claim(&token, &t).unwrap_err();
        assert!(err.as_user_message().contains("HTTPS"));
    }

    #[test]
    fn claim_403_is_compromise() {
        let url = "https://bridge.simplefin.org/simplefin/claim/abc";
        let mut t = MapTransport::new();
        t.by_url.insert(
            url.to_string(),
            TransportResponse {
                status: 403,
                body: b"nope".to_vec(),
            },
        );
        let err = SimpleFinSource.claim(&b64(url), &t).unwrap_err();
        assert!(err.as_user_message().contains("compromised"));
    }

    #[test]
    fn claim_once_saves_access_url() {
        let url = "https://bridge.simplefin.org/simplefin/claim/ok";
        let mut t = MapTransport::new();
        t.by_url.insert(
            url.to_string(),
            TransportResponse {
                status: 200,
                body: b"https://u:p@bridge.simplefin.org/simplefin".to_vec(),
            },
        );
        let secrets = SimpleFinSource.claim(&b64(url), &t).unwrap();
        assert!(secrets.inner.starts_with("https://"));
        assert_eq!(t.last_urls.lock().unwrap().len(), 1);
    }

    #[test]
    fn fetch_maps_fixture() {
        let mut t = MapTransport::new();
        t.by_url.insert(
            "https://bridge.simplefin.org/simplefin/accounts".into(),
            TransportResponse {
                status: 200,
                body: include_bytes!("../../tests/fixtures/simplefin_accounts.json").to_vec(),
            },
        );
        let secrets = ConnectionSecrets {
            inner: "https://demo:pass@bridge.simplefin.org/simplefin".into(),
        };
        let set = SimpleFinSource
            .fetch(
                &secrets,
                DateWindow {
                    start_date: 1,
                    end_date: 2,
                },
                true,
                &t,
            )
            .unwrap();
        assert_eq!(set.accounts.len(), 1);
        assert_eq!(set.accounts[0].transactions.len(), 1);
        assert_eq!(set.accounts[0].transactions[0].amount_cents, -3329343);
        assert_eq!(set.errors[0].code, "act.failed");
        assert!(!set.errors[0].message.contains('<'));
    }

    #[test]
    fn parse_keeps_raw_objects_including_unknown_fields() {
        let set = parse_account_set(include_bytes!(
            "../../tests/fixtures/simplefin_accounts.json"
        ))
        .unwrap();
        let acc = &set.accounts[0];
        let raw_txn: Value =
            serde_json::from_str(acc.transactions[0].raw.as_deref().unwrap()).unwrap();
        assert_eq!(raw_txn["id"], "12394832938403");
        assert_eq!(raw_txn["extra"]["merchant_category"], "5941");

        // The account copy keeps its own fields and `extra` but drops the transactions array,
        // since each transaction already carries its own copy.
        let raw_acc: Value = serde_json::from_str(acc.raw.as_deref().unwrap()).unwrap();
        assert_eq!(raw_acc["available-balance"], "75.23");
        assert_eq!(raw_acc["extra"]["routing"], "hidden");
        assert!(raw_acc.get("transactions").is_none());
    }

    #[test]
    fn parse_reads_institution_from_connections() {
        let set = parse_account_set(include_bytes!(
            "../../tests/fixtures/simplefin_accounts.json"
        ))
        .unwrap();
        assert_eq!(set.accounts[0].institution.as_deref(), Some("Example Bank"));

        // No org_name falls back to the login name; blank or missing leaves it unset, and an
        // account whose connection is not listed gets nothing.
        let body = |conns: &str| {
            format!(
                r#"{{"errlist":[],"connections":{conns},"accounts":[{{"id":"a","conn_id":"C1","name":"Checking","currency":"USD","balance":"1.00","balance-date":1}}]}}"#
            )
        };
        let set =
            parse_account_set(body(r#"[{"conn_id":"C1","name":"My Login"}]"#).as_bytes()).unwrap();
        assert_eq!(set.accounts[0].institution.as_deref(), Some("My Login"));
        let set =
            parse_account_set(body(r#"[{"conn_id":"C1","name":"  ","org_name":""}]"#).as_bytes())
                .unwrap();
        assert_eq!(set.accounts[0].institution, None);
        let set =
            parse_account_set(body(r#"[{"conn_id":"C2","org_name":"Other"}]"#).as_bytes()).unwrap();
        assert_eq!(set.accounts[0].institution, None);
        let set = parse_account_set(body("[]").as_bytes()).unwrap();
        assert_eq!(set.accounts[0].institution, None);
    }

    #[test]
    fn fetch_uses_https_accounts_url_without_userinfo() {
        let mut t = MapTransport::new();
        t.by_url.insert(
            "https://bridge.simplefin.org/simplefin/accounts".into(),
            TransportResponse {
                status: 200,
                body: b"{\"errlist\":[],\"accounts\":[]}".to_vec(),
            },
        );
        let secrets = ConnectionSecrets {
            inner: "https://demo:s3cret@bridge.simplefin.org/simplefin".into(),
        };
        SimpleFinSource
            .fetch(
                &secrets,
                DateWindow {
                    start_date: 1,
                    end_date: 2,
                },
                false,
                &t,
            )
            .unwrap();
        let urls = t.last_urls.lock().unwrap();
        assert!(urls.iter().all(|u| !u.contains("s3cret")));
        assert!(urls.iter().all(|u| u.starts_with("https://")));
    }
}
