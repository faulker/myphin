use dioxus::prelude::*;

use super::{TraceExchange, AI_BUSY};
use crate::ui::relative_time;
use crate::ui::status::{push_status, StatusState};
use crate::SharedStore;
use myphin::ai::{self, AiSecret, AiSettings, AiTrace, Direction};
use myphin::domain::Txn;
use myphin::money::format_cents;
use myphin::providers::{
    parse_root_certificate, url_allowed, ReqwestTransport, SimpleFinSource, TransactionSource,
};
use myphin::sync::{default_history_window, sync_connection};
use myphin::TxnQuery;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Connections,
    Ai,
    Debug,
    Log,
    Appearance,
    Passphrase,
}

/// Setup screen: bank connections, the AI categorizer, the session log, the theme,
/// and the passphrase. Categories and rules have their own screen.
#[component]
pub fn SetupView(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
    theme: Signal<String>,
) -> Element {
    let mut tab = use_signal(|| Tab::Connections);
    let _ = nonce();
    if store.read().is_none() {
        return rsx! { p { "Locked" } };
    }
    let log_len = status.read().log.len();

    rsx! {
        div { class: "setup",
            nav { class: "subnav", aria_label: "Setup sections",
                for (t, label) in [(Tab::Connections, "Connections"), (Tab::Ai, "AI"), (Tab::Debug, "Debug"), (Tab::Log, "Log"), (Tab::Appearance, "Appearance"), (Tab::Passphrase, "Passphrase")] {
                    button {
                        class: if tab() == t { "sub on" } else { "sub" },
                        onclick: move |_| tab.set(t),
                        "{label}"
                        if t == Tab::Log && log_len > 0 {
                            span { class: "badge muted", "{log_len}" }
                        }
                    }
                }
            }
            match tab() {
                Tab::Connections => rsx! { Connections { store, nonce, status, syncing } },
                Tab::Ai => rsx! { AiSetup { store, nonce, status, syncing } },
                Tab::Debug => rsx! { DebugSetup { store, nonce, status } },
                Tab::Log => rsx! { StatusLog { store, status, nonce } },
                Tab::Appearance => rsx! { Appearance { store, nonce, status, theme } },
                Tab::Passphrase => rsx! { Passphrase { store, status } },
            }
        }
    }
}

#[component]
fn Connections(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
) -> Element {
    let mut token = use_signal(String::new);
    let mut conn_name = use_signal(|| "SimpleFIN".to_string());
    let mut show_add = use_signal(|| false);
    let mut syncing = syncing;
    let mut confirm_remove = use_signal(|| None::<String>);
    let _ = nonce();

    let (conns, accounts) = store()
        .and_then(|s| {
            s.lock().ok().map(|g| {
                (
                    g.list_connections().unwrap_or_default(),
                    g.list_accounts().unwrap_or_default(),
                )
            })
        })
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let adding = conns.is_empty() || show_add();

    let connect = move |_| {
        let Some(shared) = store() else {
            push_status(status, "Locked");
            return;
        };
        let tok = token.read().trim().to_string();
        let name = conn_name.read().trim().to_string();
        if tok.is_empty() {
            push_status(status, "Paste a setup token first.");
            return;
        }
        let name = if name.is_empty() {
            "SimpleFIN".to_string()
        } else {
            name
        };
        syncing.set(Some("new".into()));
        push_status(status, "Connecting…");
        spawn(async move {
            let worker = shared.clone();
            let result = tokio::task::spawn_blocking(move || claim_and_sync(worker, tok, name))
                .await
                .unwrap_or_else(|_| Err("Sync failed.".into()));
            match result {
                Ok(msg) => {
                    let note = after_sync_phase(shared, syncing).await;
                    push_status(status, format!("{msg}{note}"));
                    token.set(String::new());
                    show_add.set(false);
                }
                Err(e) => push_status(status, e),
            }
            syncing.set(None);
            super::bump(nonce);
        });
    };

    rsx! {
        section {
            if conns.is_empty() {
                h2 { "Connect a bank" }
            } else {
                h2 { "Connections" }
            }
            ul { class: "conns",
                for c in conns.iter() {
                    {
                        let id = c.id.clone();
                        let busy = syncing().as_deref() == Some(id.as_str());
                        let any_busy = syncing().is_some();
                        let armed = confirm_remove().as_deref() == Some(id.as_str());
                        rsx! {
                            li { class: "card",
                                div { class: "card-main",
                                    strong { "{c.name}" }
                                    span { class: "hint",
                                        match c.last_sync_at {
                                            Some(ts) if busy => "Syncing…".to_string(),
                                            Some(ts) => format!("Synced {}", relative_time(ts, now)),
                                            None if busy => "Syncing…".to_string(),
                                            None => "Never synced".to_string(),
                                        }
                                    }
                                }
                                if let Some(err) = c.last_error.as_ref() {
                                    p { class: "err", "{err}" }
                                }
                                ul { class: "accts",
                                    for a in accounts.iter().filter(|a| a.connection_id == id) {
                                        {
                                            let aid = a.id.clone();
                                            let hidden = a.hidden;
                                            rsx! {
                                                li { key: "{aid}", class: if hidden { "hidden" } else { "" },
                                                    span { "{a.name}" }
                                                    if let Some(bank) = a.institution.as_ref() {
                                                        span { class: "hint-inline", "{bank}" }
                                                    }
                                                    if hidden { span { class: "tag", "hidden" } }
                                                    button {
                                                        class: "ghost small",
                                                        aria_label: if hidden { "Show account" } else { "Hide account" },
                                                        title: "Hidden accounts leave Activity, counts, and caps.",
                                                        onclick: move |_| {
                                                            if let Some(s) = store() {
                                                                match s.lock().unwrap().set_account_hidden(&aid, !hidden) {
                                                                    Ok(_) => super::bump(nonce),
                                                                    Err(e) => push_status(status, e.as_user_message()),
                                                                }
                                                            }
                                                        },
                                                        if hidden { "Show" } else { "Hide" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                div { class: "card-actions",
                                    button {
                                        class: "primary",
                                        disabled: any_busy,
                                        onclick: {
                                            let id = id.clone();
                                            move |_| start_sync(store, id.clone(), syncing, status, nonce)
                                        },
                                        if busy { "Syncing…" } else { "Sync now" }
                                    }
                                    if armed {
                                        span { class: "hint", "Removes its accounts and transactions." }
                                        button {
                                            class: "danger on",
                                            onclick: {
                                                let id = id.clone();
                                                move |_| {
                                                    if let Some(s) = store() {
                                                        match s.lock().unwrap().delete_connection(&id) {
                                                            Ok(_) => push_status(status, "Connection removed."),
                                                            Err(e) => push_status(status, e.as_user_message()),
                                                        }
                                                    }
                                                    confirm_remove.set(None);
                                                    super::bump(nonce);
                                                }
                                            },
                                            "Confirm remove"
                                        }
                                        button { class: "ghost small", onclick: move |_| confirm_remove.set(None), "Cancel" }
                                    } else {
                                        button {
                                            class: "danger",
                                            disabled: any_busy,
                                            onclick: { let id = id.clone(); move |_| confirm_remove.set(Some(id.clone())) },
                                            "Remove"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !conns.is_empty() && !show_add() {
                button { class: "ghost", onclick: move |_| show_add.set(true), "Add another connection" }
            }
            if adding {
                div { class: "card form",
                    p { class: "lede",
                        "Create a setup token at "
                        a { href: "https://bridge.simplefin.org/simplefin/create", "bridge.simplefin.org" }
                        ", paste it here, and Myphin pulls the last few months. Your bank password never touches this app."
                    }
                    label { r#for: "sf-token", "Setup token" }
                    textarea {
                        id: "sf-token",
                        value: "{token}",
                        oninput: move |e| token.set(e.value()),
                        placeholder: "Paste the token",
                        spellcheck: "false",
                        autocomplete: "off",
                        rows: "3",
                    }
                    label { r#for: "sf-name", "Name" }
                    input { id: "sf-name", value: "{conn_name}", oninput: move |e| conn_name.set(e.value()), placeholder: "SimpleFIN" }
                    div { class: "row",
                        button {
                            class: "primary",
                            disabled: syncing().is_some(),
                            onclick: connect,
                            if syncing().as_deref() == Some("new") { "Connecting…" } else { "Connect and sync" }
                        }
                        if !conns.is_empty() {
                            button { class: "ghost", onclick: move |_| show_add.set(false), "Cancel" }
                        }
                    }
                }
            }
        }
    }
}

/// AI categorizer settings and the manual run button. The key is never echoed back; the
/// field stays blank and a note says one is saved.
#[component]
fn AiSetup(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
) -> Element {
    let _ = nonce();
    let saved: AiSettings = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.ai_settings().ok()))
        .unwrap_or_default();
    let mut provider = use_signal(|| {
        saved
            .provider
            .clone()
            .unwrap_or_else(|| ai::PROVIDERS[0].provider_id().to_string())
    });
    let mut key = use_signal(String::new);
    let mut endpoint = use_signal(|| saved.endpoint.clone().unwrap_or_default());
    let mut ca_cert = use_signal(|| saved.ca_cert.clone().unwrap_or_default());
    let mut threshold = use_signal(|| format!("{}", (saved.threshold * 100.0).round() as i64));
    let mut after_sync = use_signal(|| saved.after_sync);
    let has_key = !saved.api_key.is_empty();
    let configured = saved.is_configured();
    let saved_key = saved.api_key.clone();
    let busy = syncing().is_some();
    let uncategorized = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.uncategorized_count().ok()))
        .unwrap_or(0);
    let selected = ai::categorizer_for(&provider());
    let key_help = selected.map(|p| p.key_help()).unwrap_or("");
    let needs_key = selected.map(|p| p.needs_key()).unwrap_or(true);
    let key_label = if needs_key {
        "API key"
    } else {
        "API key (optional)"
    };
    let description = selected.map(|p| p.description()).unwrap_or("");
    let warning = selected.and_then(|p| p.warning());
    let link = selected.and_then(|p| p.link());
    let default_endpoint = selected.and_then(|p| p.default_endpoint());

    let save = move |_| {
        let Some(s) = store() else {
            push_status(status, "Locked");
            return;
        };
        let pct = match threshold.read().trim().parse::<f64>() {
            Ok(p) if (0.0..=100.0).contains(&p) => p,
            _ => {
                push_status(status, "Threshold must be a number from 0 to 100.");
                return;
            }
        };
        let typed = key.read().trim().to_string();
        let api_key = if typed.is_empty() {
            saved_key.clone()
        } else {
            AiSecret(typed)
        };
        // The URL and certificate only mean something for a self-hosted provider; the fields
        // are hidden for the others and their values are not saved.
        let (endpoint, ca_cert) = if default_endpoint.is_some() {
            let url = endpoint.read().trim().trim_end_matches('/').to_string();
            if !url.is_empty() && !url_allowed(&url) {
                push_status(
                    status,
                    "Server URL must be https, or http on a local network address.",
                );
                return;
            }
            let pem = ca_cert.read().trim().to_string();
            if !pem.is_empty() && parse_root_certificate(&pem).is_none() {
                push_status(status, "Server certificate is not valid PEM.");
                return;
            }
            (
                (!url.is_empty()).then_some(url),
                (!pem.is_empty()).then_some(pem),
            )
        } else {
            (None, None)
        };
        let settings = AiSettings {
            provider: Some(provider()),
            api_key,
            endpoint,
            ca_cert,
            threshold: pct / 100.0,
            after_sync: after_sync(),
        };
        let result = s.lock().unwrap().set_ai_settings(&settings);
        match result {
            Ok(_) => {
                key.set(String::new());
                push_status(status, "AI settings saved.");
                super::bump(nonce);
            }
            Err(e) => push_status(status, e.as_user_message()),
        }
    };

    let run = move |_| start_ai_run(store, None, syncing, status, nonce);

    rsx! {
        section {
            h2 { "AI categorizer" }
            p { class: "lede",
                "Rows that no rule or remembered payee covers can be sent to an AI service, which picks a category using the descriptions in Categories & Rules. Only the payee and whether money went in or out are sent. Answers under the threshold stay uncategorized."
            }
            div { class: "card form",
                label { r#for: "ai-provider", "Provider" }
                select {
                    id: "ai-provider",
                    value: "{provider}",
                    onchange: move |e| provider.set(e.value()),
                    for p in ai::PROVIDERS.iter() {
                        option { value: "{p.provider_id()}", selected: provider() == p.provider_id(), "{p.label()}" }
                    }
                }
                if !description.is_empty() {
                    p { class: "hint",
                        "{description}"
                        if let Some((text, url)) = link {
                            " "
                            a { href: "{url}", "{text}" }
                        }
                    }
                }
                if let Some(warning) = warning {
                    p { class: "hint warn", "{warning}" }
                }
                if let Some(default_url) = default_endpoint {
                    label { r#for: "ai-endpoint", "Server URL" }
                    input {
                        id: "ai-endpoint",
                        r#type: "url",
                        value: "{endpoint}",
                        oninput: move |e| endpoint.set(e.value()),
                        placeholder: "{default_url}",
                        autocomplete: "off",
                        spellcheck: "false",
                    }
                    p { class: "hint",
                        "Leave blank for {default_url} on this machine. For a server elsewhere, use https, or http on a local network address."
                    }
                    label { r#for: "ai-ca-cert", "Server certificate (PEM, optional)" }
                    textarea {
                        id: "ai-ca-cert",
                        rows: "4",
                        value: "{ca_cert}",
                        oninput: move |e| ca_cert.set(e.value()),
                        placeholder: "-----BEGIN CERTIFICATE-----",
                        spellcheck: "false",
                    }
                    p { class: "hint",
                        "Only for a self-signed https server: paste its cert.pem here and it is trusted for this server alone."
                    }
                }
                label { r#for: "ai-key", "{key_label}" }
                input {
                    id: "ai-key",
                    r#type: "password",
                    value: "{key}",
                    oninput: move |e| key.set(e.value()),
                    placeholder: if has_key { "****************" } else { "Paste the key" },
                    autocomplete: "off",
                    spellcheck: "false",
                }
                if has_key {
                    p { class: "hint", "A key is saved. Paste a new one to replace it." }
                }
                if !key_help.is_empty() {
                    p { class: "hint", "{key_help}" }
                }
                label { r#for: "ai-threshold", "Accept when confidence is at least (%)" }
                input {
                    id: "ai-threshold",
                    class: "num cap-input",
                    r#type: "number",
                    min: "0",
                    max: "100",
                    step: "1",
                    value: "{threshold}",
                    oninput: move |e| threshold.set(e.value()),
                }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: after_sync(),
                        onchange: move |e| after_sync.set(e.checked()),
                    }
                    " Run after every sync"
                }
                div { class: "row",
                    button { class: "primary", onclick: save, "Save" }
                }
            }
            div { class: "card",
                div { class: "card-main",
                    strong { "Categorize now" }
                    span { class: "hint",
                        if uncategorized == 0 {
                            "Nothing is uncategorized."
                        } else {
                            "{uncategorized} uncategorized. Payees already answered are not asked again; rows where the AI failed earlier are retried here."
                        }
                    }
                }
                div { class: "card-actions",
                    button {
                        class: "primary",
                        disabled: busy || !configured || uncategorized == 0,
                        onclick: run,
                        if syncing().as_deref() == Some(AI_BUSY) { "Asking…" } else { "Categorize with AI" }
                    }
                }
            }
            AiDebug { store, nonce, status, syncing, configured }
        }
    }
}

/// How many matching rows the debug picker lists. Search narrows it further.
const DEBUG_PICK_LIMIT: usize = 50;

/// Debug card: pick any transaction, send it to the provider once, and show the exact request
/// body and response. Nothing is cached or applied, so it is safe on categorized rows too.
#[component]
fn AiDebug(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
    configured: bool,
) -> Element {
    let _ = nonce();
    let mut search = use_signal(String::new);
    let mut picked = use_signal(|| None::<String>);
    let mut trace = use_signal(|| None::<AiTrace>);
    let mut busy = use_signal(|| false);
    let rows: Vec<Txn> = store()
        .and_then(|s| {
            s.lock().ok().and_then(|g| {
                g.query_transactions(&TxnQuery {
                    search: search(),
                    ..Default::default()
                })
                .ok()
            })
        })
        .unwrap_or_default()
        .into_iter()
        .take(DEBUG_PICK_LIMIT)
        .collect();
    // A pick that fell out of the list (search changed) counts as no pick.
    let pick_ok = picked()
        .map(|id| rows.iter().any(|t| t.id == id))
        .unwrap_or(false);

    let send = move |_| {
        let (Some(shared), Some(id)) = (store(), picked()) else {
            return;
        };
        busy.set(true);
        trace.set(None);
        spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let transport = ReqwestTransport::new().map_err(|e| e.as_user_message())?;
                let st = shared.lock().unwrap();
                ai::trace_transaction(&st, &transport, &id).map_err(|e| e.as_user_message())
            })
            .await
            .unwrap_or_else(|_| Err("AI debug run failed.".into()));
            match result {
                Ok(t) => trace.set(Some(t)),
                Err(e) => push_status(status, e),
            }
            busy.set(false);
        });
    };

    rsx! {
        div { class: "card form ai-debug",
            div { class: "card-main",
                strong { "Debug a transaction" }
                span { class: "hint", "Sends one row to the provider and shows the exact request and response. Nothing is cached or applied." }
            }
            label { r#for: "ai-debug-search", "Find a transaction" }
            input {
                id: "ai-debug-search",
                r#type: "search",
                value: "{search}",
                placeholder: "Payee, notes, or amount",
                oninput: move |e| search.set(e.value()),
            }
            label { r#for: "ai-debug-pick", "Transaction" }
            select {
                id: "ai-debug-pick",
                value: picked().unwrap_or_default(),
                onchange: move |e| {
                    let v = e.value();
                    picked.set(if v.is_empty() { None } else { Some(v) });
                },
                option { value: "", selected: !pick_ok,
                    if rows.is_empty() { "No matching transactions" } else { "Pick a transaction…" }
                }
                for t in rows.iter() {
                    option { key: "{t.id}", value: "{t.id}", selected: picked().as_deref() == Some(t.id.as_str()),
                        "{debug_pick_label(t)}"
                    }
                }
            }
            div { class: "row",
                button {
                    class: "primary",
                    disabled: busy() || syncing().is_some() || !configured || !pick_ok,
                    onclick: send,
                    if busy() { "Asking…" } else { "Send to AI" }
                }
                if !configured {
                    span { class: "hint", "Finish AI setup first." }
                }
            }
            if let Some(t) = trace() {
                div { class: "trace",
                    p { class: "hint",
                        "Sent: "
                        code { "{t.input.title}" }
                        match t.input.direction {
                            Direction::In => " (money in)",
                            Direction::Out => " (money out)",
                        }
                    }
                    for (i, x) in t.exchanges.iter().enumerate() {
                        TraceExchange {
                            key: "{i}",
                            exchange: x.clone(),
                            attempt: (t.exchanges.len() > 1).then_some(i + 1),
                        }
                    }
                    if t.exchanges.is_empty() {
                        p { class: "hint", "No request was sent." }
                    }
                    p { class: "trace-result",
                        "{t.outcome()}"
                    }
                }
            }
        }
    }
}

/// "2026-09-03 · STARBUCKS · -$4.50" for the debug picker, with the category when it has one.
fn debug_pick_label(t: &Txn) -> String {
    let date = chrono::DateTime::from_timestamp(t.posted_at, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".into());
    let mut s = format!("{date} · {} · {}", t.payee, format_cents(t.amount_cents));
    if let Some(c) = &t.category_name {
        s.push_str(&format!(" · {c}"));
    }
    s
}

/// Kick off an AI pass from the UI thread. `ids` limits it to those rows (one row, or the rows
/// on screen); `None` runs every candidate. Marks the AI busy so the top bar shows it, toasts
/// the result, and re-renders when done. Shared by Setup → AI and the Activity screen.
pub fn start_ai_run(
    store: Signal<Option<SharedStore>>,
    ids: Option<Vec<String>>,
    mut syncing: Signal<Option<String>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
) {
    let Some(shared) = store() else {
        push_status(status, "Locked");
        return;
    };
    syncing.set(Some(AI_BUSY.into()));
    push_status(status, "Asking the AI…");
    spawn(async move {
        let result = tokio::task::spawn_blocking(move || run_ai(shared, ids))
            .await
            .unwrap_or_else(|_| Err("AI run failed.".into()));
        match result {
            Ok(msg) => push_status(status, msg),
            Err(e) => push_status(status, e),
        }
        syncing.set(None);
        super::bump(nonce);
    });
}

/// The manual AI pass. Same runtime rule as [`claim_and_sync`].
fn run_ai(store: SharedStore, ids: Option<Vec<String>>) -> Result<String, String> {
    let transport = ReqwestTransport::new().map_err(|e| e.as_user_message())?;
    let st = store.lock().unwrap();
    match ids {
        Some(ids) => ai::categorize_txns(&st, &transport, &ids),
        None => ai::categorize_uncategorized(&st, &transport),
    }
    .map(|r| r.summary())
    .map_err(|e| e.as_user_message())
}

/// The optional AI pass after a sync, run as its own blocking task so the top bar can show
/// the AI indicator while it works. Returns text to append to the sync toast, empty when the
/// pass is off or no key is saved.
async fn after_sync_phase(store: SharedStore, mut syncing: Signal<Option<String>>) -> String {
    let wanted = store
        .lock()
        .ok()
        .and_then(|g| g.ai_settings().ok())
        .is_some_and(|s| s.after_sync && s.is_configured());
    if !wanted {
        return String::new();
    }
    syncing.set(Some(AI_BUSY.into()));
    tokio::task::spawn_blocking(move || {
        let transport = match ReqwestTransport::new() {
            Ok(t) => t,
            Err(e) => return format!(" {}", e.as_user_message()),
        };
        let st = store.lock().unwrap();
        match ai::run_after_sync(&st, &transport) {
            Ok(Some(r)) => format!(" {}", r.summary()),
            Ok(None) => String::new(),
            Err(e) => format!(" {}", e.as_user_message()),
        }
    })
    .await
    .unwrap_or_else(|_| " AI run failed.".into())
}

/// Every status this session, newest first. Transaction changes carry an Undo button until used.
/// Debug section: one switch that adds "Raw data" and "AI trace" to every transaction's editor.
#[component]
fn DebugSetup(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
) -> Element {
    let _ = nonce();
    let enabled = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.debug_enabled().ok()))
        .unwrap_or(false);
    let toggle = move |e: Event<FormData>| {
        let on = e.checked();
        let Some(s) = store() else {
            push_status(status, "Locked");
            return;
        };
        let result = s.lock().unwrap().set_debug_enabled(on);
        match result {
            Ok(()) => push_status(
                status,
                if on {
                    "Debug views on."
                } else {
                    "Debug views off."
                },
            ),
            Err(err) => push_status(status, err.as_user_message()),
        }
        super::bump(nonce);
    };
    rsx! {
        section {
            h2 { "Debug" }
            p { class: "lede",
                "Shows what is under each transaction: the bank's own data for the row and its account, and the exact request and response when the AI categorizer was asked about its payee. Nothing is sent anywhere."
            }
            div { class: "card form",
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: enabled,
                        onchange: toggle,
                    }
                    " Show debug views on transactions"
                }
                p { class: "hint", "Adds Raw data and AI trace buttons to the editor of every row in Activity. Raw data fills in on the next sync for rows imported before this version." }
            }
        }
    }
}

/// Appearance: the ledger's theme, the screen it opens on, and Activity's opening chip.
#[component]
fn Appearance(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    mut theme: Signal<String>,
) -> Element {
    let _ = nonce();
    let current = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.theme().ok()))
        .unwrap_or_else(|| myphin::store::DEFAULT_THEME.to_string());
    let save = move |e: Event<FormData>| {
        let id = e.value();
        let Some(s) = store() else {
            push_status(status, "Locked");
            return;
        };
        let label = myphin::store::THEMES
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(_, name)| *name)
            .unwrap_or("Ledger");
        match s.lock().unwrap().set_theme(&id) {
            Ok(()) => {
                theme.set(id);
                push_status(status, format!("Theme set to {label}."));
            }
            Err(err) => push_status(status, err.as_user_message()),
        }
        super::bump(nonce);
    };
    let open_on = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.open_screen().ok()))
        .unwrap_or_else(|| myphin::store::DEFAULT_OPEN_SCREEN.to_string());
    let activity_on = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.activity_scope().ok()))
        .unwrap_or_else(|| myphin::store::DEFAULT_ACTIVITY_SCOPE.to_string());
    let save_open = move |e: Event<FormData>| {
        save_choice(
            store,
            status,
            nonce,
            e.value(),
            myphin::store::OPEN_SCREENS,
            |s, id| s.set_open_screen(id),
            |label| format!("This ledger will open on {label}."),
        );
    };
    let save_scope = move |e: Event<FormData>| {
        save_choice(
            store,
            status,
            nonce,
            e.value(),
            myphin::store::ACTIVITY_SCOPES,
            |s, id| s.set_activity_scope(id),
            |label| format!("Activity will open on {label}."),
        );
    };
    rsx! {
        section {
            h2 { "Appearance" }
            p { class: "lede",
                "The look of this ledger. It stays with the folder, and locking keeps the last choice until the app closes."
            }
            div { class: "card form",
                label { r#for: "theme", "Theme" }
                select {
                    id: "theme",
                    value: "{current}",
                    onchange: save,
                    for (id, label) in myphin::store::THEMES {
                        option { key: "{id}", value: "{id}", selected: current == *id, "{label}" }
                    }
                }
            }
            div { class: "card form",
                label { r#for: "open-screen", "Open on" }
                select {
                    id: "open-screen",
                    value: "{open_on}",
                    onchange: save_open,
                    for (id, label) in myphin::store::OPEN_SCREENS {
                        option { key: "{id}", value: "{id}", selected: open_on == *id, "{label}" }
                    }
                }
                p { class: "hint", "Used the next time this ledger unlocks." }
                label { r#for: "activity-scope", "Activity opens on" }
                select {
                    id: "activity-scope",
                    value: "{activity_on}",
                    onchange: save_scope,
                    for (id, label) in myphin::store::ACTIVITY_SCOPES {
                        option { key: "{id}", value: "{id}", selected: activity_on == *id, "{label}" }
                    }
                }
                p { class: "hint", "The chip Activity starts on, and what Reset returns to." }
            }
        }
    }
}

/// Change the passphrase that encrypts this ledger. The previous file stays until the next unlock.
#[component]
fn Passphrase(store: Signal<Option<SharedStore>>, status: Signal<StatusState>) -> Element {
    let mut current = use_signal(String::new);
    let mut new_pass = use_signal(String::new);
    let mut confirm = use_signal(String::new);

    rsx! {
        section {
            h2 { "Passphrase" }
            p { class: "lede",
                "Re-encrypts this ledger. A copy under the current passphrase stays in the data folder until you unlock with the new one. A forgotten passphrase still cannot be recovered."
            }
            div { class: "card form",
                label { r#for: "pass-current", "Current passphrase" }
                input {
                    id: "pass-current",
                    r#type: "password",
                    value: "{current}",
                    autocomplete: "current-password",
                    spellcheck: "false",
                    oninput: move |e| current.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            apply_passphrase(store, status, current, new_pass, confirm);
                        }
                    },
                }
                label { r#for: "pass-new", "New passphrase" }
                input {
                    id: "pass-new",
                    r#type: "password",
                    value: "{new_pass}",
                    autocomplete: "new-password",
                    spellcheck: "false",
                    oninput: move |e| new_pass.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            apply_passphrase(store, status, current, new_pass, confirm);
                        }
                    },
                }
                label { r#for: "pass-confirm", "Confirm new passphrase" }
                input {
                    id: "pass-confirm",
                    r#type: "password",
                    value: "{confirm}",
                    autocomplete: "new-password",
                    spellcheck: "false",
                    oninput: move |e| confirm.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            apply_passphrase(store, status, current, new_pass, confirm);
                        }
                    },
                }
                div { class: "row",
                    button {
                        class: "primary",
                        onclick: move |_| {
                            apply_passphrase(store, status, current, new_pass, confirm);
                        },
                        "Change passphrase"
                    }
                }
                p { class: "hint",
                    "The copy is {myphin::store::LEDGER_BACKUP}. If the new passphrase will not unlock, quit, replace ledger.enc with that file, and use the previous passphrase."
                }
            }
        }
    }
}

/// Check the two new fields, then re-encrypt. Clears the form only after a successful change.
fn apply_passphrase(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    mut current: Signal<String>,
    mut new_pass: Signal<String>,
    mut confirm: Signal<String>,
) {
    let Some(s) = store() else {
        push_status(status, "Locked");
        return;
    };
    // Copy the strings out so the signal borrows end before the fields are cleared.
    let current_pass = current.read().clone();
    let next = new_pass.read().clone();
    let again = confirm.read().clone();
    if next != again {
        push_status(status, "Those passphrases do not match.");
        return;
    }
    let result = s.lock().unwrap().change_passphrase(&current_pass, &next);
    match result {
        Ok(()) => {
            current.set(String::new());
            new_pass.set(String::new());
            confirm.set(String::new());
            push_status(
                    status,
                    format!(
                        "Passphrase changed. {} will be removed the next time you unlock with the new passphrase.",
                        myphin::store::LEDGER_BACKUP
                    ),
                );
        }
        Err(err) => push_status(status, err.as_user_message()),
    }
}

/// Save one of the opening choices and toast its label.
fn save_choice(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
    id: String,
    choices: &[(&str, &str)],
    write: impl FnOnce(&myphin::Store, &str) -> Result<(), myphin::Error>,
    toast: impl FnOnce(&str) -> String,
) {
    let Some(s) = store() else {
        push_status(status, "Locked");
        return;
    };
    let label = choices
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, name)| *name)
        .unwrap_or("that");
    match write(&s.lock().unwrap(), &id) {
        Ok(()) => push_status(status, toast(label)),
        Err(err) => push_status(status, err.as_user_message()),
    }
    super::bump(nonce);
}

#[component]
fn StatusLog(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
) -> Element {
    let _ = nonce();
    let log = status.read().log.clone();
    let timeout = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.status_timeout_secs().ok()))
        .unwrap_or(myphin::store::DEFAULT_STATUS_TIMEOUT_SECS);
    // Saves on change (blur or Enter). A bad value toasts the problem and the field snaps back.
    let save_timeout = move |e: Event<FormData>| {
        let Some(s) = store() else {
            push_status(status, "Locked");
            return;
        };
        let result = match e.value().trim().parse::<u64>() {
            Ok(secs) => s.lock().unwrap().set_status_timeout_secs(secs),
            Err(_) => Err(myphin::Error::user(
                "Timeout must be a whole number of seconds.",
            )),
        };
        match result {
            Ok(()) => push_status(
                status,
                format!("Status messages now hide after {}s.", e.value().trim()),
            ),
            Err(err) => push_status(status, err.as_user_message()),
        }
        super::bump(nonce);
    };
    rsx! {
        section { class: "status-log",
            h2 { "Status log" }
            p { class: "hint", "Every sync result, change, and error from this session. Cleared when the app closes." }
            div { class: "card form",
                label { r#for: "status-timeout", "Hide status messages after" }
                div { class: "row",
                    input {
                        id: "status-timeout",
                        class: "num cap-input",
                        r#type: "number",
                        min: "{myphin::store::MIN_STATUS_TIMEOUT_SECS}",
                        max: "{myphin::store::MAX_STATUS_TIMEOUT_SECS}",
                        step: "1",
                        value: "{timeout}",
                        onchange: save_timeout,
                    }
                    span { class: "hint", "seconds. Messages with Undo stay 3s longer. Hovering a message holds it open." }
                }
            }
            if log.is_empty() {
                p { class: "empty", "Nothing yet." }
            } else {
                ol {
                    for e in log.iter().rev() {
                        li { key: "{e.id}",
                            span { class: if e.undone { "undone" } else { "" }, "{e.message}" }
                            if e.undone {
                                span { class: "tag", "undone" }
                            } else if e.undo.is_some() {
                                button {
                                    class: "ghost small",
                                    onclick: { let id = e.id; move |_| super::status::undo_entry(store, status, nonce, id) },
                                    "Undo"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Claim a setup token and import accounts. Must run off the Dioxus/tokio thread:
/// reqwest blocking drops its own runtime, which panics inside async.
fn claim_and_sync(store: SharedStore, token: String, name: String) -> Result<String, String> {
    let transport = ReqwestTransport::new().map_err(|e| e.as_user_message())?;
    let source = SimpleFinSource;
    let secrets = source
        .claim(&token, &transport)
        .map_err(|e| e.as_user_message())?;
    let st = store.lock().unwrap();
    let id = st
        .add_connection(source.source_id(), &name, &secrets)
        .map_err(|e| e.as_user_message())?;
    drop(secrets);
    let w = default_history_window();
    let report = sync_connection(&st, &id, &source, &transport, w.start_date, w.end_date)
        .map_err(|e| e.as_user_message())?;
    Ok(format!(
        "Connected. {} new, {} updated.{}",
        report.stats.inserted,
        report.stats.updated,
        if report.errors.is_empty() {
            String::new()
        } else {
            format!(" {}", report.errors.join(" "))
        }
    ))
}

/// Kick off a sync for one connection from the UI thread: marks it busy, toasts progress, and
/// re-renders when done. Shared by the Setup card and the top bar's quick sync.
pub fn start_sync(
    store: Signal<Option<SharedStore>>,
    id: String,
    mut syncing: Signal<Option<String>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
) {
    let Some(shared) = store() else {
        return;
    };
    syncing.set(Some(id.clone()));
    push_status(status, "Syncing…");
    spawn(async move {
        let worker = shared.clone();
        let result = tokio::task::spawn_blocking(move || resync(worker, id))
            .await
            .unwrap_or_else(|_| Err("Sync failed.".into()));
        match result {
            Ok(msg) => {
                let note = after_sync_phase(shared, syncing).await;
                push_status(status, format!("{msg}{note}"));
            }
            Err(e) => push_status(status, e),
        }
        syncing.set(None);
        super::bump(nonce);
    });
}

/// Re-fetch one connection. Same runtime rule as [`claim_and_sync`].
fn resync(store: SharedStore, id: String) -> Result<String, String> {
    let transport = ReqwestTransport::new().map_err(|e| e.as_user_message())?;
    let st = store.lock().unwrap();
    let w = default_history_window();
    let report = sync_connection(
        &st,
        &id,
        &SimpleFinSource,
        &transport,
        w.start_date,
        w.end_date,
    )
    .map_err(|e| e.as_user_message())?;
    Ok(format!(
        "Synced. {} new, {} updated.{}",
        report.stats.inserted,
        report.stats.updated,
        if report.errors.is_empty() {
            String::new()
        } else {
            format!(" {}", report.errors.join(" "))
        }
    ))
}
