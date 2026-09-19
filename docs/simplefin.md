# SimpleFIN protocol packet

Single source of truth for Myphin's first `TransactionSource` implementation.
Sources: [Developer Guide](https://beta-bridge.simplefin.org/info/developers), [SimpleFIN Protocol](https://www.simplefin.org/protocol.html) (v2).

## Parties

- **User** shares read-only bank data without giving Myphin their bank password.
- **App** (Myphin) stores an Access URL and pulls accounts/transactions.
- **Server** is usually [SimpleFIN Bridge](https://bridge.simplefin.org/) (`https://beta-bridge.simplefin.org` for demo tokens).

Myphin never sees bank credentials. The Access URL **is** a long-lived credential. Treat it like a password.

## Connection flow

1. Send the user to create a Setup Token:
   - Production: `https://bridge.simplefin.org/simplefin/create`
   - Demo tokens: [Developer Guide](https://beta-bridge.simplefin.org/info/developers) (refresh the page for a new one).
2. User pastes the Setup Token into Myphin. It is a Base64-encoded HTTPS claim URL.
3. Decode the token. Reject anything that is not `https://`. POST with empty body (`Content-Length: 0`) **once**.
4. Response body is the Access URL, which embeds Basic Auth credentials:
   `https://user:password@host/simplefin`
5. Persist the Access URL in the encrypted secret store. The Setup Token is now spent and will never work again.
6. Fetch data with GET `{access_url}/accounts` using the URL's Basic Auth. Always pass `version=2`.
7. The user can revoke the Access URL on the Bridge at any time.

Claim is not retryable. If claim succeeds and we crash before saving, the user must generate a new Setup Token.

## GET /accounts

Relative to the Access URL root.

| Parameter | Required | Notes |
| --- | --- | --- |
| `version` | yes (for us) | Always `2`. |
| `start-date` | no | Unix epoch. Inclusive. Transactions on or after. |
| `end-date` | no | Unix epoch. Exclusive. Transactions before. |
| `pending` | no | `1` includes pending (if the institution supports it). Default omits pending. |
| `account` | no | Repeatable. Filter to one remote account id. |
| `balances-only` | no | `1` skips transaction payloads. |

Authentication: HTTP Basic Auth from the Access URL userinfo. Follow redirects (`-L`).

### Response: Account Set

- `errlist` (required): structured errors. Always show sanitized messages to the user.
- `errors` (deprecated): string list. Ignore if `errlist` is present.
- `connections`: institutions / logins.
- `accounts`: accounts plus optional `transactions`.

### Connection

`conn_id`, `name` (the login), `org_id`, `sfin_url`, optional `org_name` and `org_url`. This is
the only place the bank's name appears. One Access URL can hold several connections, so Myphin
joins each account to its connection by `conn_id` and keeps `org_name` (falling back to `name`)
as `accounts.institution`, shown beside the account name.

### Account

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Unique **within the connection**. |
| `name` | string | Display name. |
| `conn_id` | string | Parent connection. |
| `currency` | string | ISO 4217 (`USD`) or a custom-currency URL. |
| `balance` | numeric string | Never parse as float in domain code. Convert to integer cents. |
| `available-balance` | numeric string | Optional. |
| `balance-date` | unix epoch | |
| `transactions` | array | Ordered by `posted`. |
| `extra` | object | Opaque. Store, do not require. |

### Transaction

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Unique **within the account**. May be reused across accounts. |
| `posted` | unix epoch | `0` if pending. |
| `amount` | numeric string | Positive = money in. Negative = money out. |
| `description` | string | Payee / memo from the bank. |
| `transacted_at` | unix epoch | Optional. When it happened vs when it posted. |
| `pending` | bool | Default false. |
| `extra` | object | Opaque. |

Raw copies: every account and transaction object is also stored verbatim as JSON
(`accounts.raw_json`, `transactions.raw_json`; the account copy drops its `transactions` array).
Unknown fields and `extra` survive that way even though the parser reads only the columns above.
Setup → Debug shows them on each row. Rows imported before schema 6 fill in on the next sync.

Dedup key for Myphin: `(source_id, remote_account_id, remote_txn_id)` where `source_id` is the local SimpleFIN connection id, `remote_account_id` is account `id`, `remote_txn_id` is transaction `id`.

Do **not** dedup on date + amount + description. Recurring charges collide; pending→posted corrections miss.

## HTTP status

| Status | Where | Meaning | User-facing action |
| --- | --- | --- | --- |
| 200 | claim / accounts | OK | Persist / import. |
| 403 | POST claim | Token missing, already used, or possibly stolen | Tell the user the token may be compromised; they should disable it on the Bridge and create a new one. |
| 403 | GET accounts | Bad credentials or access revoked | Prompt reconnect. |
| 402 | GET accounts | Payment required on the Bridge | Show sanitized body / errlist. |

Unknown `errlist` codes: fall back to the prefix (`gen.`, `con.`, `act.`).

| Code prefix | Meaning |
| --- | --- |
| `gen.` | General |
| `gen.api` | App bug (developer), still show sanitized msg |
| `gen.auth` | Auth to SimpleFIN server |
| `con.` / `con.auth` | Connection-level (includes `conn_id`) |
| `act.` / `act.failed` / `act.missingdata` | Account-level (includes `account_id`). Retry later. |

Sanitize every server string before display (HTML, control chars). Institution text is untrusted.

## Limits (Bridge)

- Intended cadence: daily updates. **≤ 24 requests per day**. A little leeway for first setup.
- Quotas refill through the day. Exceeding warnings then disables the Access Token.
- `GET /accounts` (all accounts) has one quota. `GET /accounts?account=...` has its own.
- Date window (`end-date` − `start-date`) hard **max 90 days**; Bridge now warns (and may later cap) at **45 days**. Use 45.
- History depth varies by institution.
- Overlap windows by **~5 days** so you do not miss late-posted transactions.
- If polling on a timer, pick a **random minute** (not `:00`) to avoid Bridge load spikes.

Myphin v1: sync on launch, on demand, and about every 6 hours while the app is open, with a random minute offset. Pull 90 days of history in ≤45 day windows with 5-day overlap.

## Security checklist (required)

1. HTTPS only. Reject decoded claim URLs and Access URLs that are not `https://`.
2. Verify TLS certificates. No custom CA disable, no `danger_accept_invalid_certs`.
3. Claim: handle 403 as possible compromise.
4. Accounts: handle 403 as revoked/bad creds.
5. Store Access URLs at least as securely as the transaction database (encrypted data folder).
6. Never log Access URLs, Basic Auth userinfo, Setup Tokens, or raw claim URLs.
7. Show sanitized `errlist` messages to the user.
8. Do not ship Access URLs to crash reports, analytics, or UI copy/debug dumps.

## Amounts

Parse numeric strings with a decimal money parser into `i64` cents. Reject values that do not convert cleanly. Never use `f64` for money.

## Custom currencies

If `currency` is a URL, GET it (HTTPS, certs on) for `{ name, abbr }`. Sanitize. v1 may keep the raw URL as the code and skip the extra fetch until Setup needs a label.

## Provider mapping (Myphin)

`TransactionSource` for SimpleFIN:

- `claim(setup_token)` → decode, HTTPS check, POST once, return Access URL as `ConnectionSecrets` (never in logs).
- `fetch(secrets, window)` → GET `/accounts?version=2&start-date=&end-date=&pending=1`, map into normalized accounts/transactions.

Normalized transaction always includes `(source_id, remote_account_id, remote_txn_id)`.

## Live demo

Refresh https://beta-bridge.simplefin.org/info/developers for a one-shot demo Setup Token. Claim once, fetch `/accounts?version=2`, then fetch again and assert no duplicate rows for the same triple.

## Out of scope for the transport layer

Categorization, budgets, splits, and user overrides belong in domain/importer, not the SimpleFIN client.
