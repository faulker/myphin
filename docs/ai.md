# AI categorization

A last-resort step after rules and payee memory. Rows that neither covers can be sent to an
AI service, which picks one category using the descriptions from Categories & Rules.

## Precedence

`user` > `rule` > `memory` > `ai`. `transactions.categorized_by` records which one set the
category. The AI pass only touches rows with `category_id IS NULL`, and `auto_categorize_new`
lets rules and memory overwrite `ai` rows on later syncs. A hand edit sets `user` and drops
the `ai` tag. AI answers never seed payee memory (only user categorizations do).

## Entry points

`categorize_uncategorized` runs every candidate row (Setup → AI button). `run_after_sync`
does the same when the toggle is on, minus rows flagged as failed (below). `categorize_txns(ids)`
limits the pass to given ids: Activity's "Ask AI" on one row and "Categorize N with AI" for the
rows on screen. Non-candidate ids are skipped, and answers apply only to the given ids, never to
other rows sharing the payee (those pick the cached answer up on the next full run).

## Failures

When the provider call for a payee errors, the run stops (progress so far is kept) and two
things are written. `ai_answers` gets a row for the payee with `error` set and the recorded
exchanges, so Activity's "AI trace" shows the request and what came back. `ai_answer()` ignores
such rows, so the payee is asked again next time. Each transaction in the group gets
`ai_failed_at`, which the after-sync pass and Activity's on-screen bulk button skip. Only two
things retry a flagged row: "Ask AI" on that row, and Setup → AI → "Categorize with AI". A
successful answer for the payee clears the flag on the rows it covered.

## What is sent

Per distinct payee: the payee text and whether money went in or out. No amounts, accounts,
dates, or bank identifiers. Categories go along as the option list: name plus description
(blank descriptions are sent as `null`), and a fixed `other` option for "nothing fits". A
sub-category is sent by its full label, `Parent › Child`, so the model sees where it sits.
Off-budget categories are in the list like any other. Categories with Send to AI off
are omitted from the option list (rules, payee memory, and hand edits can still assign
them). Unticking a parent also unticks its sub-categories, so a child drops off the
list too and cannot be turned back on until its parent is. If every category is off,
the pass errors until one is turned back on.

## Threshold and cache

Settings (`Store::ai_settings`) live in the ledger's `meta` table: provider id, API key,
threshold (0..1, default 0.70), the run-after-sync toggle, and for a self-hosted provider the
server URL (`ai_endpoint`) and an optional trusted certificate (`ai_ca_cert`, PEM). The key,
URL, and certificate are encrypted at rest with the rest of the ledger, like SimpleFIN Access
URLs, and the key never appears in logs or messages. A provider whose
`Categorizer::needs_key()` is `false` (currently only LMR) is considered configured with no
key at all; `AiSettings::is_configured()` is what Setup and `run_after_sync` check.

`ai_answers` caches one answer per `normalize_payee` key, including "other" and
below-threshold answers, so a payee is asked once. Each answer also keeps the round trips that
produced it (`exchanges`: a JSON array of `AiExchange`, recorded through `RecordingTransport`,
so headers and the key are never stored; an answer saved before exchanges were kept has
`exchanges IS NULL` and does not count as cached, so that payee is asked once more).
`Store::ai_record_for(txn_id)` reads the answer and
trail back for a row's payee; Activity shows it under "AI trace" when Setup → Debug is on. Lowering the threshold re-applies cached
answers without network calls. Any category add, rename, re-parent, description change,
Send to AI toggle, or delete clears the cache because the option list the model saw changed.
The in-budget flag does not, since it changes nothing the model sees.

## Debug trace

Setup → AI → "Debug a transaction" calls `ai::trace_transaction(store, transport, txn_id)`
(`src/ai/debug.rs`). It wraps the real transport in `RecordingTransport`, which keeps each
request's method, URL, and body and each response's status and body (headers are dropped, so
the key never reaches the screen), then runs the provider once on that row. The `AiTrace` it
returns carries the `CategorizeInput`, the exchanges, the parsed `CategoryGuess` and category
name, the highest entry in `answers.category.probabilities` when the response has one (the
rating, separate from confidence), how long the call took (retries included), or the sanitized
error. It works on any row, including categorized ones, and writes
nothing: no `ai_answers` entry and no category change.

## Adding a provider

Implement `ai::Categorizer` (`src/ai/mod.rs`) and add the unit struct to `ai::PROVIDERS`.
That is the whole registration: Setup lists providers from that slice, `categorizer_for`
resolves the stored id, and `run.rs` never names a provider.

```rust
pub trait Categorizer: Send + Sync {
    fn provider_id(&self) -> &'static str;   // stored in settings, never changes
    fn label(&self) -> &'static str;         // dropdown text
    fn key_help(&self) -> &'static str;      // hint under the key field
    fn needs_key(&self) -> bool { true }     // false for a provider that works with no key
    fn description(&self) -> &'static str;   // what it is, shown under the dropdown
    fn warning(&self) -> Option<&'static str> { None }  // gold caveat under the description
    fn link(&self) -> Option<(&'static str, &'static str)> { None }  // (text, URL) after it
    fn default_endpoint(&self) -> Option<&'static str> { None }      // Some = self-hosted
    fn categorize(&self, settings: &AiSettings, input: &CategorizeInput,
                  options: &[CategoryOption], transport: &dyn Transport)
        -> Result<CategoryGuess, Error>;
}
```

Rules for an implementation:

- Go through `providers::Transport` (`TransportRequest` carries headers, a JSON body, an
  optional extra root certificate, and an optional whole-request `timeout`) so tests can use
  `ScriptedTransport` and the URL guard applies. The guard (`providers::url_allowed`) accepts
  any `https://` URL, and `http://` only to hosts that cannot be reached from the public
  internet: loopback, RFC 1918 and link-local addresses, IPv6 unique-local, single-label
  hostnames, and `.local` / `.lan` / `.home` / `.internal` / `.home.arpa` names. Public plain
  HTTP is refused. `timeout` defaults to 30s (reqwest's blocking client default); a local model
  that may take tens of seconds must set a longer one.
- `settings` carries the key plus, for a provider with a `default_endpoint()`, the saved
  server URL (`AiSettings::endpoint_or(default)`) and certificate (`ca_cert_pem()`). Setup
  shows the URL and certificate fields only for such providers and validates both on Save.
- Return `CategoryGuess { category_id: None, .. }` for "nothing fits". Confidence is 0..1.
- Errors are `Error::user(..)` with text safe to show; never include the key or the URL.
  Sanitize any provider text with `sanitize_user_text`.
- Retry only on rate limiting; everything else stops the run (progress so far is kept).
- Override `needs_key()` to return `false` only when the provider genuinely works without one
  (e.g. it only listens on loopback); `AiSettings::is_configured()` then treats an empty key
  as configured for that provider.

Two providers speak the same System One wire protocol (request/response shapes, retry loop,
error mapping), factored into `ai::systemone::post_systemone(endpoint, key, root_cert_pem,
timeout, input, options, transport, unreachable_msg, timeout_msg)`. A new provider against
that same protocol should call it rather than reimplement the wire format;
`src/ai/typesafe.rs` and `src/ai/lmr.rs` are both thin wrappers around it.

`src/ai/typesafe.rs` is the reference: `POST https://api.typesafe.ai/v1/systemone`, bearer
auth, one `choice` question whose criteria are the categories, answer read from
`answers.category.{choice, confidence}`, backoff on 429/529.

## Self-hosted model

`ai::Lmr` (`src/ai/lmr.rs`, provider id `lmr`, shown as "LMR") talks to
[lmr-rs](https://github.com/faulker/lmr-rs), a native Rust server you
run yourself. It serves the same System One API as typesafe.ai, so Myphin uses it the same way.
Nothing is sent to a third party.

A model you host will not match typesafe.ai. Expect more misses and more below-threshold
answers; the threshold keeps those from being applied. For the best local results, serve
[openbmb/MiniCPM5-2B-GGUF](https://huggingface.co/openbmb/MiniCPM5-2B-GGUF) (the `minicpm5-2b`
variant, Q4_K_M, about 1.5 GB). Other catalog checkpoints work with the same API and categorize
worse.

### Run the server

```sh
git clone https://github.com/faulker/lmr-rs
cd lmr-rs
cargo build --release --features metal   # macOS; omit --features metal for CPU
./target/release/lmr-rs models download minicpm5-2b
./target/release/lmr-rs serve            # http://127.0.0.1:8321
```

Leave `serve` running. After the model is on disk it does not need the network. Pin the
variant so a later start does not ask which checkpoint to load (`lmr-rs config init` writes
`~/.config/lmr-rs/config.toml`):

```toml
[model]
variant = "minicpm5-2b"
```

`model.repo` can point at another Hugging Face id or a local directory, and `--filename` picks
a different GGUF in that repo (for example `MiniCPM5-2B-Q8_0.gguf`). See the lmr-rs README for
the rest of the catalog, TLS, and running it as a service.

### Point Myphin at it

Setup → AI → "LMR". The screen shows a short description, the GitHub link, and
a warning that local results trail typesafe.ai and that MiniCPM5-2B-GGUF is the model to serve.

- Server URL: blank means `http://127.0.0.1:8321` (this machine). For a server elsewhere enter
  its base URL; the provider appends `/v1/systemone`. It must pass `url_allowed`: https
  anywhere, or http on a local network address.
- API key: only when the server has `server.api_key` set; sent as a bearer token. Reaching
  another machine needs `public = true` and a key (lmr-rs refuses a public bind without one).
- Server certificate: lmr-rs writes a self-signed `cert.pem` when `tls.enabled = true` without
  a `cert`/`key`. myphin's HTTP client trusts only public roots, so paste that PEM here; it is
  added as a trusted root for this provider's requests only. A public certificate (Let's
  Encrypt, a reverse proxy) needs nothing.

Threshold, run-after-sync, and Debug a transaction work the same as with typesafe.ai. A CPU
`lmr-rs` can take on the order of ten seconds per forward pass, and a large category list is
more than one pass, so this provider waits up to three minutes (reqwest's default 30s would
cut it off and look like the server was down). Rebuild with `--features metal` on a Mac when
you want that much faster.

`tests/live_lmr.rs` drives the real provider through `ReqwestTransport` against a running
server: `cargo test -- --ignored live_lmr`. Set `LMR_URL`, `LMR_KEY`, and `LMR_CERT` (path
to the server's `cert.pem`) to point it at a self-hosted TLS server.
