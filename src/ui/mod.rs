mod activity;
mod categories;
mod month;
mod rules;
mod setup;
mod status;
mod unlock;

use dioxus::prelude::*;

use crate::SharedStore;
use myphin::ai::AiExchange;
use myphin::TxnQuery;
use status::StatusState;

/// `syncing` sentinel while the AI categorizer runs, from the button or after a sync. The top
/// bar shows an indicator whenever this is set.
pub const AI_BUSY: &str = "ai";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Screen {
    Month,
    Activity,
    Categories,
    Setup,
}

/// Re-render every screen that reads the store. Call after any write.
pub fn bump(mut nonce: Signal<u64>) {
    let v = nonce();
    nonce.set(v + 1);
}

/// Screen shown when a ledger unlocks, from a saved id. Unknown ids open Budget.
pub fn screen_for(id: &str) -> Screen {
    match id {
        "activity" => Screen::Activity,
        "categories" => Screen::Categories,
        _ => Screen::Month,
    }
}

/// Activity query for a saved chip id. Unknown ids list everything.
pub fn activity_scope_query(scope: &str) -> TxnQuery {
    match scope {
        "uncategorized" => TxnQuery {
            uncategorized_only: true,
            ..Default::default()
        },
        "ai" => TxnQuery {
            ai_only: true,
            ..Default::default()
        },
        "excluded" => TxnQuery {
            excluded_only: true,
            ..Default::default()
        },
        _ => TxnQuery::default(),
    }
}

#[component]
pub fn App() -> Element {
    let mut store = use_signal(|| None::<SharedStore>);
    let mut theme = use_signal(|| myphin::store::DEFAULT_THEME.to_string());
    // A newly opened ledger replaces the painted theme. Locking leaves the signal alone,
    // so Unlock keeps the last choice for the rest of the session.
    let store_for_theme = store;
    use_effect(move || {
        let Some(shared) = store_for_theme() else {
            return;
        };
        let Ok(guard) = shared.lock() else {
            return;
        };
        let Ok(id) = guard.theme() else {
            return;
        };
        if theme.peek().as_str() != id {
            theme.set(id);
        }
    });
    let mut screen = use_signal(|| Screen::Month);
    let mut filter = use_signal(|| activity_scope_query(myphin::store::DEFAULT_ACTIVITY_SCOPE));
    let status = use_signal(StatusState::default);
    let nonce = use_signal(|| 0u64);
    // The connection id being synced right now, if any. Shared so the top bar and Setup agree.
    let syncing = use_signal(|| None::<String>);
    // How far Activity was scrolled when the user last left it, restored on return.
    let mut activity_scroll = use_signal(|| 0.0f64);
    let _ = nonce();

    // Leave Activity for another screen, noting the scroll offset first so coming back lands in the same spot.
    let mut leave_activity = move |to: Screen| {
        if *screen.peek() != Screen::Activity {
            screen.set(to);
            return;
        }
        spawn(async move {
            if let Some(y) = document::eval("return window.scrollY;")
                .await
                .ok()
                .and_then(|v| v.as_f64())
            {
                activity_scroll.set(y);
            }
            screen.set(to);
        });
    };

    let todo = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.uncategorized_count().ok()))
        .unwrap_or(0);
    let conns = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.list_connections().ok()))
        .unwrap_or_default();
    let theme_id = theme();

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/main.css") }
        document::Link { rel: "icon", href: asset!("/assets/icon.svg") }
        div { class: "app", "data-theme": "{theme_id}",
        if store.read().is_none() {
            unlock::Unlock { store, screen, filter }
        } else {
            div { class: "shell",
                header { class: "top",
                    div { class: "brand",
                        img { class: "logo", src: asset!("/assets/icon.svg"), alt: "" }
                        span { class: "mark", "My" }
                        span { "phin" }
                    }
                    nav { aria_label: "Screens",
                        button {
                            class: if *screen.read() == Screen::Month { "nav on" } else { "nav" },
                            onclick: move |_| leave_activity(Screen::Month),
                            "Budget"
                        }
                        button {
                            class: if *screen.read() == Screen::Activity { "nav on" } else { "nav" },
                            onclick: move |_| {
                                let home = activity_home(store);
                                // A different filter means a different list, so start that one from the top.
                                if *filter.peek() != home {
                                    activity_scroll.set(0.0);
                                }
                                filter.set(home);
                                screen.set(Screen::Activity);
                            },
                            "Activity"
                            if todo > 0 {
                                span { class: "badge", title: "Uncategorized", "{todo}" }
                            }
                        }
                        button {
                            class: if *screen.read() == Screen::Categories { "nav on" } else { "nav" },
                            onclick: move |_| leave_activity(Screen::Categories),
                            "Categories & Rules"
                        }
                    }
                    div { class: "top-right",
                        if syncing.read().as_deref() == Some(AI_BUSY) {
                            span { class: "ai-busy", role: "status", aria_live: "polite",
                                span { class: "dot", "aria-hidden": "true" }
                                "Categorizing with AI…"
                            }
                        }
                        if !conns.is_empty() {
                            select {
                                class: "quick-sync",
                                aria_label: "Sync a connection",
                                title: "Sync a connection now",
                                disabled: syncing.read().is_some(),
                                // Shows the connection while it syncs, then drops back to the placeholder.
                                value: syncing.read().clone().unwrap_or_default(),
                                onchange: move |e| {
                                    let id = e.value();
                                    if !id.is_empty() {
                                        setup::start_sync(store, id, syncing, status, nonce);
                                    }
                                },
                                option { value: "", disabled: true, selected: true, "Sync…" }
                                for c in conns.iter() {
                                    option { key: "{c.id}", value: "{c.id}",
                                        if syncing.read().as_deref() == Some(c.id.as_str()) { "Syncing {c.name}…" } else { "{c.name}" }
                                    }
                                }
                            }
                        }
                        button {
                            class: if *screen.read() == Screen::Setup { "nav cog on" } else { "nav cog" },
                            aria_label: "Setup",
                            title: "Setup",
                            onclick: move |_| leave_activity(Screen::Setup),
                            svg {
                                width: "18",
                                height: "18",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "1.8",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                "aria-hidden": "true",
                                circle { cx: "12", cy: "12", r: "3" }
                                path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h0a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v0a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" }
                            }
                        }
                        button {
                            class: "nav lock",
                            title: "Close the ledger and go back to Unlock",
                            onclick: move |_| {
                                activity_scroll.set(0.0);
                                store.set(None);
                            },
                            "Lock"
                        }
                    }
                }
                main { class: "body",
                    match *screen.read() {
                        Screen::Month => rsx! { month::MonthView { store, screen, filter, nonce, status, activity_scroll } },
                        Screen::Activity => rsx! { activity::ActivityView { store, filter, nonce, status, syncing, scroll: activity_scroll } },
                        Screen::Categories => rsx! { categories::CategoriesView { store, nonce, status } },
                        Screen::Setup => rsx! { setup::SetupView { store, nonce, status, syncing, theme } },
                    }
                }
                Toast { store, status, nonce }
            }
        }
        }
    }
}

/// Floating status popup. A bar along its bottom edge shrinks over the timeout saved in
/// Setup → Log and hides the toast when it runs out; hovering pauses it. Close dismisses
/// without touching the log. Undo reverses the change it reports, when there is one.
#[component]
fn Toast(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
) -> Element {
    let mut status = status;
    let _ = nonce();
    let base = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.status_timeout_secs().ok()))
        .unwrap_or(myphin::store::DEFAULT_STATUS_TIMEOUT_SECS);
    let toast = status.read().toast.clone();
    rsx! {
        // A keyed list of at most one, so a newer message replaces the element and its bar
        // starts from full instead of inheriting this one's clock.
        for toast in toast {
            div {
                key: "{toast.id}",
                class: "toast",
                role: "status",
                title: "Hover to keep this open",
                p { "{toast.message}" }
                if toast.undo.is_some() {
                    button {
                        class: "toast-undo",
                        onclick: move |_| status::undo_entry(store, status, nonce, toast.id),
                        "Undo"
                    }
                }
                button {
                    class: "toast-close",
                    aria_label: "Dismiss status",
                    onclick: move |_| status.write().dismiss(),
                    "Close"
                }
                div {
                    class: "toast-bar",
                    "aria-hidden": "true",
                    style: "animation-duration: {status::toast_secs(base, toast.undo.is_some())}s;",
                    onanimationend: move |_| status.write().expire(toast.id),
                }
            }
        }
    }
}

/// The Activity chip this ledger opens on, or All when the ledger is locked or the value is bad.
fn activity_home(store: Signal<Option<SharedStore>>) -> TxnQuery {
    let scope = store()
        .and_then(|s| s.lock().ok().and_then(|g| g.activity_scope().ok()))
        .unwrap_or_else(|| myphin::store::DEFAULT_ACTIVITY_SCOPE.to_string());
    activity_scope_query(&scope)
}

/// "September 2026" for a (year, month) pair.
pub fn month_label(year: i32, month: u32) -> String {
    format!("{} {year}", month_name(month))
}

pub fn month_name(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        _ => "December",
    }
}

/// Step a (year, month) pair by one month in either direction.
pub fn step_month(year: i32, month: u32, forward: bool) -> (i32, u32) {
    match (month, forward) {
        (12, true) => (year + 1, 1),
        (1, false) => (year - 1, 12),
        (m, true) => (year, m + 1),
        (m, false) => (year, m - 1),
    }
}

/// One recorded HTTP round trip: Request and Response bodies, each with a Copy button.
#[component]
pub(crate) fn TraceExchange(exchange: AiExchange, attempt: Option<usize>) -> Element {
    let status_hint = match exchange.status {
        Some(code) => format!(" HTTP {code}"),
        None => " no response (network error)".to_string(),
    };
    rsx! {
        if let Some(n) = attempt {
            p { class: "hint", "Attempt {n}" }
        }
        h3 {
            "Request"
            span { class: "hint-inline", " {exchange.method} {exchange.url}" }
        }
        TraceField {
            kind: "request",
            text: exchange.request.clone(),
            placeholder_when_empty: false,
        }
        h3 {
            "Response"
            span { class: "hint-inline", "{status_hint}" }
        }
        TraceField {
            kind: "response",
            text: exchange.response.clone(),
            placeholder_when_empty: true,
        }
    }
}

/// Request or response body with a Copy button in the top-right corner of the field.
#[component]
fn TraceField(kind: &'static str, text: String, placeholder_when_empty: bool) -> Element {
    let mut copied = use_signal(|| false);
    let display = trace_field_display(&text, placeholder_when_empty).to_string();
    let payload = text.clone();
    rsx! {
        div { class: "trace-field",
            button {
                class: "ghost small trace-copy",
                aria_label: "Copy {kind}",
                onclick: move |_| {
                    copy_to_clipboard(&payload);
                    copied.set(true);
                },
                if copied() { "Copied" } else { "Copy" }
            }
            pre { "{display}" }
        }
    }
}

/// Body shown in a trace field. Empty responses read as "(empty)"; Copy still gets the raw body.
fn trace_field_display(text: &str, placeholder_when_empty: bool) -> &str {
    if placeholder_when_empty && text.is_empty() {
        "(empty)"
    } else {
        text
    }
}

/// JSON-encode `text` as a JavaScript string literal.
fn js_string_literal(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// Put `text` on the system clipboard from the webview.
/// The payload is JSON-encoded into a string literal so it cannot break out of the script;
/// `document::eval` here runs only this fixed clipboard helper, the same way scroll uses eval.
fn copy_to_clipboard(text: &str) {
    let json = js_string_literal(text);
    document::eval(&format!(
        r#"(async () => {{
            const t = {json};
            try {{
                await navigator.clipboard.writeText(t);
            }} catch (e) {{
                const ta = document.createElement('textarea');
                ta.value = t;
                ta.setAttribute('readonly', '');
                ta.style.position = 'fixed';
                ta.style.left = '-9999px';
                document.body.appendChild(ta);
                ta.select();
                document.execCommand('copy');
                document.body.removeChild(ta);
            }}
        }})()"#
    ));
}

/// "Just now", "12 min ago", "3 h ago", or a date, for last-sync stamps.
pub fn relative_time(ts: i64, now: i64) -> String {
    let d = now - ts;
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{} min ago", d / 60)
    } else if d < 86_400 {
        format!("{} h ago", d / 3600)
    } else if d < 7 * 86_400 {
        format!("{} d ago", d / 86_400)
    } else {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%b %-d, %Y").to_string())
            .unwrap_or_else(|| "-".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_month_wraps_years() {
        assert_eq!(step_month(2026, 12, true), (2027, 1));
        assert_eq!(step_month(2026, 1, false), (2025, 12));
        assert_eq!(step_month(2026, 6, true), (2026, 7));
        assert_eq!(step_month(2026, 6, false), (2026, 5));
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_800_000_000;
        assert_eq!(relative_time(now - 5, now), "just now");
        assert_eq!(relative_time(now - 600, now), "10 min ago");
        assert_eq!(relative_time(now - 7200, now), "2 h ago");
        assert_eq!(relative_time(now - 3 * 86_400, now), "3 d ago");
        assert!(relative_time(now - 30 * 86_400, now).contains(", 20"));
    }

    #[test]
    fn trace_field_display_placeholder_only_when_empty() {
        assert_eq!(trace_field_display("", true), "(empty)");
        assert_eq!(trace_field_display("", false), "");
        assert_eq!(trace_field_display("{}", true), "{}");
    }

    #[test]
    fn screen_for_maps_saved_ids_and_falls_back_to_budget() {
        assert_eq!(screen_for("month"), Screen::Month);
        assert_eq!(screen_for("activity"), Screen::Activity);
        assert_eq!(screen_for("categories"), Screen::Categories);
        assert_eq!(screen_for("nope"), Screen::Month);
        assert_eq!(screen_for(""), Screen::Month);
    }

    #[test]
    fn activity_scope_query_maps_chips_and_falls_back_to_all() {
        assert!(activity_scope_query("uncategorized").uncategorized_only);
        assert!(activity_scope_query("ai").ai_only);
        assert!(activity_scope_query("excluded").excluded_only);
        assert_eq!(activity_scope_query("all"), TxnQuery::default());
        assert_eq!(activity_scope_query("nope"), TxnQuery::default());
    }

    #[test]
    fn js_string_literal_escapes_quotes_and_newlines() {
        assert_eq!(js_string_literal(r#"{"a":"b"}"#), r#""{\"a\":\"b\"}""#);
        assert_eq!(js_string_literal("line1\nline2"), r#""line1\nline2""#);
        assert_eq!(js_string_literal("say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(js_string_literal(""), r#""""#);
    }
}
