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
Off-budget categories are in the list like any other.

## Threshold and cache

Settings (`Store::ai_settings`) live in the ledger's `meta` table: provider id, API key,
threshold (0..1, default 0.70), and the run-after-sync toggle. The key is encrypted at rest with
the rest of the ledger, like SimpleFIN Access URLs, and never appears in logs or messages.

`ai_answers` caches one answer per `normalize_payee` key, including "other" and
below-threshold answers, so a payee is asked once. Each answer also keeps the round trips that
produced it (`exchanges`: a JSON array of `AiExchange`, recorded through `RecordingTransport`,
so headers and the key are never stored; an answer saved before exchanges were kept has
`exchanges IS NULL` and does not count as cached, so that payee is asked once more).
`Store::ai_record_for(txn_id)` reads the answer and
trail back for a row's payee; Activity shows it under "AI trace" when Setup → Debug is on. Lowering the threshold re-applies cached
answers without network calls. Any category add, rename, re-parent, description change, or
delete clears the cache because the option list the model saw changed. The in-budget flag
does not, since it changes nothing the model sees.

## Debug trace

Setup → AI → "Debug a transaction" calls `ai::trace_transaction(store, transport, txn_id)`
(`src/ai/debug.rs`). It wraps the real transport in `RecordingTransport`, which keeps each
request's method, URL, and body and each response's status and body (headers are dropped, so
the key never reaches the screen), then runs the provider once on that row. The `AiTrace` it
returns carries the `CategorizeInput`, the exchanges, the parsed `CategoryGuess` and category
name, or the sanitized error. It works on any row, including categorized ones, and writes
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
    fn categorize(&self, key: &AiSecret, input: &CategorizeInput,
                  options: &[CategoryOption], transport: &dyn Transport)
        -> Result<CategoryGuess, Error>;
}
```

Rules for an implementation:

- Go through `providers::Transport` (`TransportRequest` carries headers and a JSON body) so
  tests can use `ScriptedTransport` and the HTTPS-only guard applies.
- Return `CategoryGuess { category_id: None, .. }` for "nothing fits". Confidence is 0..1.
- Errors are `Error::user(..)` with text safe to show; never include the key or the URL.
  Sanitize any provider text with `sanitize_user_text`.
- Retry only on rate limiting; everything else stops the run (progress so far is kept).

`src/ai/typesafe.rs` is the reference: `POST https://api.typesafe.ai/v1/systemone`, bearer
auth, one `choice` question whose criteria are the categories, answer read from
`answers.category.{choice, confidence}`, backoff on 429/529.
