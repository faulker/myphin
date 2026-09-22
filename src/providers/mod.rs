//! Extensible bank-data sources. SimpleFIN is the first implementation.

mod simplefin;

pub use simplefin::{MapTransport, ScriptedTransport, SimpleFinSource};

use std::time::Duration;

use url::Url;

use crate::error::Error;

/// reqwest's blocking client default. Used when `TransportRequest::timeout` is `None`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// User-facing text `ReqwestTransport` returns on a client timeout, so callers can remap it.
pub const TRANSPORT_TIMEOUT_MSG: &str = "The request timed out.";

/// Whether a URL is safe for a provider request: any `https://` URL, or `http://` to a host
/// that cannot be reached from the public internet (loopback, RFC 1918 and link-local
/// addresses, IPv6 unique-local, single-label hostnames, and `.local` / `.lan` / `.home` /
/// `.internal` / `.home.arpa` names), for a self-hosted model server such as `lmr-rs`.
/// Rejects lookalikes like `http://127.0.0.1.evil.com`.
pub fn url_allowed(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }
            Some(url::Host::Domain(host)) => is_private_hostname(host),
            None => false,
        },
        _ => false,
    }
}

/// Parse a PEM certificate for use as an extra trusted root. `None` when it is not valid PEM.
pub fn parse_root_certificate(pem: &str) -> Option<reqwest::Certificate> {
    reqwest::Certificate::from_pem(pem.trim().as_bytes()).ok()
}

/// Hostnames that only resolve on a local network.
fn is_private_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if host == "localhost" || !host.contains('.') {
        return true;
    }
    [".local", ".lan", ".home", ".internal", ".home.arpa"]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

/// Inclusive start / exclusive end, unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateWindow {
    pub start_date: i64,
    pub end_date: i64,
}

/// Secrets for a connection. Display/Debug redacts the credential.
#[derive(Clone)]
pub struct ConnectionSecrets {
    /// Provider-specific credential. For SimpleFIN this is the Access URL.
    pub inner: String,
}

impl std::fmt::Debug for ConnectionSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionSecrets(***)")
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedAccount {
    pub remote_id: String,
    pub conn_id: String,
    pub name: String,
    /// The bank the account lives at, when the source says (SimpleFIN `org.name`).
    pub institution: Option<String>,
    pub currency: String,
    pub balance_cents: i64,
    pub available_cents: Option<i64>,
    pub balance_date: i64,
    pub transactions: Vec<NormalizedTxn>,
    /// The account object exactly as the source sent it (minus its transactions), as JSON.
    /// Kept so nothing the bank said is lost, even fields Myphin does not read yet.
    pub raw: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NormalizedTxn {
    pub remote_id: String,
    pub posted: i64,
    pub transacted_at: Option<i64>,
    pub amount_cents: i64,
    pub description: String,
    pub pending: bool,
    /// The transaction object exactly as the source sent it, as JSON.
    pub raw: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceErrorItem {
    pub code: String,
    pub message: String,
    pub conn_id: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountSet {
    pub errors: Vec<SourceErrorItem>,
    pub accounts: Vec<NormalizedAccount>,
}

pub struct TransportRequest {
    pub method: &'static str,
    pub url: String,
    pub basic_user: Option<String>,
    pub basic_pass: Option<String>,
    pub query: Vec<(String, String)>,
    pub follow_redirects: bool,
    /// Extra headers, e.g. a bearer token. Never logged.
    pub headers: Vec<(String, String)>,
    /// JSON body for POSTs. `None` sends an empty body.
    pub body: Option<Vec<u8>>,
    /// Extra trusted root certificate (PEM) for this request only, for a self-hosted server
    /// with a self-signed certificate. Never logged.
    pub root_cert_pem: Option<String>,
    /// Whole-request timeout, including the response body. `None` uses
    /// [`DEFAULT_REQUEST_TIMEOUT`] (reqwest's 30s blocking default). Local model inference
    /// needs longer than that.
    pub timeout: Option<Duration>,
}

impl Default for TransportRequest {
    fn default() -> Self {
        Self {
            method: "GET",
            url: String::new(),
            basic_user: None,
            basic_pass: None,
            query: vec![],
            follow_redirects: false,
            headers: vec![],
            body: None,
            root_cert_pem: None,
            timeout: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransportResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// HTTP used by providers. Implementations must not log URLs (they may contain creds).
pub trait Transport: Send + Sync {
    fn send(&self, req: &TransportRequest) -> Result<TransportResponse, Error>;
}

/// A bank / aggregator that can claim a token and fetch transactions.
pub trait TransactionSource: Send + Sync {
    fn source_id(&self) -> &'static str;
    fn claim(
        &self,
        setup_token: &str,
        transport: &dyn Transport,
    ) -> Result<ConnectionSecrets, Error>;
    fn fetch(
        &self,
        secrets: &ConnectionSecrets,
        window: DateWindow,
        include_pending: bool,
        transport: &dyn Transport,
    ) -> Result<AccountSet, Error>;
}

/// Blocking reqwest + rustls. Used in production.
pub struct ReqwestTransport;

impl ReqwestTransport {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }
}

impl Transport for ReqwestTransport {
    fn send(&self, req: &TransportRequest) -> Result<TransportResponse, Error> {
        if !url_allowed(&req.url) {
            return Err(Error::user("Refusing non-HTTPS URL."));
        }
        let private_http = req.url.starts_with("http://");
        let mut builder = reqwest::blocking::Client::builder()
            .use_rustls_tls()
            .https_only(!private_http)
            .timeout(req.timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT))
            .redirect(if req.follow_redirects {
                reqwest::redirect::Policy::limited(10)
            } else {
                reqwest::redirect::Policy::none()
            });
        if let Some(pem) = req
            .root_cert_pem
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let cert = parse_root_certificate(pem)
                .ok_or_else(|| Error::user("Server certificate is not valid PEM."))?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder
            .build()
            .map_err(|_| Error::Internal(crate::error::InternalError::Http))?;
        let mut builder = match (req.method, &req.body) {
            ("POST", Some(body)) => client
                .post(&req.url)
                .header("Content-Type", "application/json")
                .body(body.clone()),
            ("POST", None) => client.post(&req.url).header("Content-Length", "0"),
            ("GET", _) => client.get(&req.url),
            (other, _) => {
                return Err(Error::user(format!("unsupported method {other}")));
            }
        };
        if let (Some(u), Some(p)) = (&req.basic_user, &req.basic_pass) {
            builder = builder.basic_auth(u, Some(p));
        }
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        for (k, v) in &req.query {
            builder = builder.query(&[(k, v)]);
        }
        let resp = builder.send().map_err(|e| {
            if e.is_timeout() {
                Error::user(TRANSPORT_TIMEOUT_MSG)
            } else {
                Error::user("Network error talking to the remote service.")
            }
        })?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .map_err(|_| Error::Internal(crate::error::InternalError::Http))?
            .to_vec();
        Ok(TransportResponse { status, body })
    }
}

/// Bridge-recommended max span for one GET /accounts. Hard cap is still 90;
/// 45 is the range they warn (and may later enforce) on.
pub const MAX_WINDOW_DAYS: i64 = 45;
/// Overlap so late-posted transactions are not missed across chunk boundaries.
pub const OVERLAP_DAYS: i64 = 5;

/// Split a long range into ≤`max_days` windows with overlap.
pub fn chunk_windows(start: i64, end: i64, max_days: i64, overlap_days: i64) -> Vec<DateWindow> {
    if end <= start {
        return vec![];
    }
    let max_secs = max_days * 86400;
    let overlap_secs = overlap_days * 86400;
    let mut windows = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let window_end = (cursor + max_secs).min(end);
        windows.push(DateWindow {
            start_date: cursor,
            end_date: window_end,
        });
        if window_end >= end {
            break;
        }
        let next = window_end - overlap_secs;
        cursor = if next <= cursor { window_end } else { next };
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_max_and_overlap() {
        let start = 0;
        let end = 200 * 86400;
        let w = chunk_windows(start, end, MAX_WINDOW_DAYS, OVERLAP_DAYS);
        assert!(w.len() >= 5);
        for win in &w {
            assert!(win.end_date - win.start_date <= MAX_WINDOW_DAYS * 86400);
        }
        for pair in w.windows(2) {
            assert!(pair[1].start_date < pair[0].end_date);
        }
        assert_eq!(w.first().unwrap().start_date, start);
        assert_eq!(w.last().unwrap().end_date, end);
    }

    #[test]
    fn url_allowed_accepts_https_and_private_http() {
        assert!(url_allowed("http://127.0.0.1:8321/x"));
        assert!(url_allowed("http://localhost/x"));
        assert!(url_allowed("http://[::1]:8321/x"));
        assert!(url_allowed("https://api.example.com"));
        for host in [
            "10.0.0.5",
            "172.16.4.2",
            "192.168.1.20:8321",
            "169.254.1.1",
            "[fd00::1]",
            "[fe80::1]",
        ] {
            assert!(url_allowed(&format!("http://{host}/x")), "{host}");
        }
        for host in [
            "nas",
            "laya.local",
            "laya.lan",
            "server.home",
            "box.internal",
            "pi.home.arpa",
            "NAS.Local.",
        ] {
            assert!(url_allowed(&format!("http://{host}:8321/x")), "{host}");
        }
    }

    #[test]
    fn url_allowed_rejects_public_http_and_other_schemes() {
        assert!(!url_allowed("http://example.com"));
        assert!(!url_allowed("http://127.0.0.1.evil.com"));
        assert!(!url_allowed("http://8.8.8.8/x"));
        assert!(!url_allowed("http://172.32.0.1/x"));
        assert!(!url_allowed("http://[2001:db8::1]/x"));
        assert!(!url_allowed("http://laya.local.example.com/x"));
        assert!(!url_allowed("ftp://127.0.0.1/x"));
        assert!(!url_allowed("not a url"));
    }

    #[test]
    fn ninety_day_history_fits_recommended_chunks() {
        let start = 0;
        let end = 90 * 86400;
        let w = chunk_windows(start, end, MAX_WINDOW_DAYS, OVERLAP_DAYS);
        assert_eq!(w.len(), 3);
        for win in &w {
            assert!(win.end_date - win.start_date <= MAX_WINDOW_DAYS * 86400);
        }
        assert_eq!(w.last().unwrap().end_date, end);
    }

    #[test]
    fn request_timeout_is_reported_as_user_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = ReqwestTransport::new().unwrap();
        let err = t
            .send(&TransportRequest {
                method: "POST",
                url: format!("http://{addr}/v1/systemone"),
                body: Some(b"{}".to_vec()),
                timeout: Some(Duration::from_millis(200)),
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err.as_user_message(), TRANSPORT_TIMEOUT_MSG);
        // Keep the listener alive until the client gives up, so the port stays bound.
        drop(listener);
    }
}
