# Myphin

Personal finance tracker for a folder you control. Syncs bank data through [SimpleFIN](https://www.simplefin.org/protocol.html), categorizes with keyboard-first Activity (learns payees as you go; wildcard rules like `*costco*` (Categories & Rules) override, with a live preview of what they match; one rule can hold several patterns, edited as a list where each one can be changed or removed on its own), and tracks monthly category caps. Categories can nest one level (`Investment › Fees`), a parent's spend rolling up its sub-categories', and any category can be kept out of the budget so its rows are categorized without touching caps or the month's spent. Rules can also mark matching rows as transfers, credit card payments, loan payments, income, or excluded (payments are transfers with a label, so they never count as spending; an Excluded chip in Activity lists everything excluded so it can be included again), can be created straight from a transaction in Activity (or a transaction can be added to an existing rule, with the pattern edited to add wildcards first), edited later in Categories & Rules, and apply to every matching row the moment they are added or changed. Any account can be hidden from Setup. Rows that no rule or remembered payee covers can be sent to an AI service ([typesafe.ai](https://typesafe.ai), or a model you host with [lmr-rs](https://github.com/faulker/lmr-rs) on this machine or one on your network, no API key needed (it will not match typesafe.ai; [openbmb/MiniCPM5-2B-GGUF](https://huggingface.co/openbmb/MiniCPM5-2B-GGUF) is the best local model); more behind one trait; see `docs/ai.md`), which picks a category from your category descriptions and only applies it above a confidence threshold you set (70% by default). One Rust crate, [Dioxus](https://dioxuslabs.com/) desktop UI, no npm.

## Setup

- Rust 1.93+ (`rustup`)
- [Dioxus CLI](https://dioxuslabs.com/learn/0.7/getting_started/): `cargo install dioxus-cli`
- macOS: no extra packages. Linux: WebKitGTK (see Dioxus desktop docs). Windows: WebView2.

```bash
git clone <this-repo>
cd myphin
cargo test
dx serve --desktop
# or
cargo run
```

First launch: pick a data folder and a passphrase. An empty folder creates a new encrypted ledger (`ledger.enc`). The passphrase is never stored. Put that folder in iCloud Drive / Syncthing if you want a backup.

Get a SimpleFIN setup token from [SimpleFIN Bridge](https://bridge.simplefin.org/simplefin/create) (demo tokens: [developer page](https://beta-bridge.simplefin.org/info/developers)), paste it in Setup (the cog at the top right), Connect and sync. After that, the Sync… dropdown in the top bar re-syncs any connection without leaving the current screen.

Optional: in Setup → AI pick a provider, paste its API key (for "LMR" the key is optional and there are Server URL and certificate fields instead; blank means an `lmr-rs` on this machine), set the confidence threshold, and choose whether it runs after every sync. Add a short description to each category in Categories & Rules (that is what the model reads). Untick Send to AI on a category to keep it off that list (unticking a parent also unticks its sub-categories); rules and hand edits can still assign it. "Categorize with AI" handles whatever is still uncategorized; in Activity, "Ask AI" in a row's editor sends just that row, and the Uncategorized list has a button for the rows on screen; the rows show an `ai` tag and the AI chip in Activity lists them for review. Only the payee and the money-in/out direction are sent. To see exactly what goes out and comes back, "Debug a transaction" in Setup → AI sends any one row and shows the request JSON, the response, the highest-rated category, and how long the call took, without changing anything. A model you host will not match typesafe.ai. For the best local results, serve [openbmb/MiniCPM5-2B-GGUF](https://huggingface.co/openbmb/MiniCPM5-2B-GGUF) (`lmr-rs models download minicpm5-2b`, then `lmr-rs serve`). A CPU build can take tens of seconds per payee; a Metal build (`--features metal`) is much faster. Hosting steps are in `docs/ai.md`.

Setup → Debug adds two buttons to every row's editor in Activity: Raw data (the bank's own JSON for the transaction and its account, kept verbatim on every sync) and AI trace (the exact request and response recorded when the AI was asked about that payee). Both read from the ledger and send nothing.

Setup → Appearance picks the look: Ledger (the default), Ink, Paper, or Newsprint. It also chooses the screen the ledger opens on (Budget by default) and the Activity chip that list starts on (All by default). Both are saved with the ledger.

Setup → Passphrase changes the passphrase. The ledger is re-encrypted. `ledger.enc.backup` keeps a copy under the previous passphrase until the next successful unlock with the new one. If that unlock fails, quit, replace `ledger.enc` with `ledger.enc.backup`, and use the previous passphrase. There is still no recovery if you forget both.

## Build

```bash
dx bundle --desktop     # release bundle
cargo build --release
```

## Test

```bash
cargo test
cargo test -- --ignored   # live SimpleFIN demo token (network)
```

## Layout

- `src/providers` — `TransactionSource` trait + SimpleFIN, `Transport` HTTP abstraction
- `src/ai` — `Categorizer` trait, the shared System One wire code, typesafe.ai and the self-hosted lmr-rs provider, the AI pass
- `src/sync` — windowed fetch, importer, dedup
- `src/store` — encrypted SQLite
- `src/domain` — categories, caps, rules, splits
- `src/ui` — Budget / Activity / Categories & Rules / Setup
- `assets` — `main.css`, and the app icon: `icon.svg` is the source, `icon.png` is the Dock icon on `cargo run`, `icon.icns` is what `dx bundle` ships
- `docs/simplefin.md` — protocol packet
- `docs/ui-spec.md` — screens and keys
- `docs/ai.md` — AI categorization, adding a provider

## Security

- The data folder holds `ledger.enc` (Argon2id + XChaCha20-Poly1305). The passphrase is never stored. Changing it in Setup re-encrypts the file under a new salt and leaves `ledger.enc.backup` (the previous passphrase) until the next successful unlock.
- SimpleFIN Access URLs and AI keys live in that ledger, not in logs (`ConnectionSecrets` and `AiSecret` debug-print as `***`).
- Claim POSTs do not follow redirects. All provider URLs must be HTTPS, or plain HTTP to a local network address (loopback, private and link-local IPs, `.local`-style names) for a self-hosted model server; a pasted certificate is trusted for that server only.
- While the app is open, `.workspace.sqlite` exists unlocked in the data folder. It is deleted on exit. Don't sync that file; sync `ledger.enc`.

## Keys

In Activity, `↑`/`↓` move, type a category name (a unique prefix is enough) and press Enter. `Space` selects, `⌘E` excludes, `⌘⌫` twice deletes, `/` jumps to search. Search matches payee, notes, or an amount; filter by account, category, and month. Full key table in `docs/ui-spec.md`.
