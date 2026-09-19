use dioxus::prelude::*;

use crate::ui::rules::{
    join_patterns, parse_rule_target, rule_added_message, rule_extended_message, rule_summary,
    PatternList, RulePreview,
};
use crate::ui::setup::start_ai_run;
use crate::ui::status::{push_status, push_undoable, StatusState, Undo};
use crate::ui::{default_filter, month_label};
use crate::SharedStore;
use myphin::domain::{match_category_typeahead, AiRecord, Category, Txn};
use myphin::money::format_cents;
use myphin::{PaymentKind, Rule, RuleAction, TxnPatch, TxnQuery};

/// Whether the AI pass would consider this row: uncategorized and not flagged as something
/// else. Mirrors `Store::list_ai_candidates` so buttons only show where a run can do anything.
fn ai_candidate(t: &Txn) -> bool {
    t.category_id.is_none() && !t.excluded && !t.is_transfer && !t.is_income && !t.has_splits
}

/// Rule form default for a row: its category if it has one, else the flag it already carries.
fn default_rule_target(txn: &Txn) -> String {
    match &txn.category_id {
        Some(c) => format!("cat:{c}"),
        None if txn.payment == Some(PaymentKind::Card) => "card_payment".into(),
        None if txn.payment == Some(PaymentKind::Loan) => "loan_payment".into(),
        None if txn.is_transfer => "transfer".into(),
        None if txn.is_income => "income".into(),
        None if txn.excluded => "exclude".into(),
        None => String::new(),
    }
}

/// The patch behind one "Mark as…" choice in the bulk bar; `None` for the placeholder.
fn bulk_mark_patch(what: &str) -> Option<TxnPatch> {
    Some(match what {
        "transfer" => TxnPatch {
            transfer_account_id: Some(Some("manual".into())),
            payment: Some(None),
            ..Default::default()
        },
        "card_payment" => TxnPatch {
            payment: Some(Some(PaymentKind::Card)),
            ..Default::default()
        },
        "loan_payment" => TxnPatch {
            payment: Some(Some(PaymentKind::Loan)),
            ..Default::default()
        },
        "income" => TxnPatch {
            income: Some(true),
            ..Default::default()
        },
        "not_transfer" => TxnPatch {
            transfer_account_id: Some(None),
            ..Default::default()
        },
        "not_income" => TxnPatch {
            income: Some(false),
            ..Default::default()
        },
        _ => return None,
    })
}

/// The editor's "Mark as" value for a row: one flag, in the order the tags show it.
fn current_mark(txn: &Txn) -> &'static str {
    match txn.payment {
        Some(PaymentKind::Card) => "card_payment",
        Some(PaymentKind::Loan) => "loan_payment",
        None if txn.is_transfer => "transfer",
        None if txn.is_income => "income",
        None if txn.excluded => "exclude",
        None => "",
    }
}

/// Flag changes for moving a row from one "Mark as" value to another, or `None` when it did
/// not change. Every flag is reset so a row ends up with exactly the chosen one.
fn mark_patch(from: &str, to: &str) -> Option<TxnPatch> {
    if from == to {
        return None;
    }
    let mut p = TxnPatch {
        transfer_account_id: Some(None),
        income: Some(false),
        excluded: Some(false),
        ..Default::default()
    };
    match to {
        "transfer" => {
            p.transfer_account_id = Some(Some("manual".into()));
            p.payment = Some(None);
        }
        "card_payment" => {
            p.transfer_account_id = Some(Some("manual".into()));
            p.payment = Some(Some(PaymentKind::Card));
        }
        "loan_payment" => {
            p.transfer_account_id = Some(Some("manual".into()));
            p.payment = Some(Some(PaymentKind::Loan));
        }
        "income" => p.income = Some(true),
        "exclude" => p.excluded = Some(true),
        _ => {}
    }
    Some(p)
}

/// What a "Mark as" change did, for the toast and log: "Marked COSTCO as transfer."
fn mark_message(payee: &str, to: &str) -> String {
    let label = match to {
        "transfer" => "transfer",
        "card_payment" => "credit card payment",
        "loan_payment" => "loan payment",
        "income" => "income",
        "exclude" => "excluded",
        _ => return format!("Unmarked {payee}."),
    };
    format!("Marked {payee} as {label}.")
}

/// Which rows a keyboard command acts on: the selection if there is one, else the cursor row.
fn targets(selected: &[String], txns: &[Txn], cursor: usize) -> Vec<String> {
    if !selected.is_empty() {
        return selected.to_vec();
    }
    txns.get(cursor)
        .map(|t| vec![t.id.clone()])
        .unwrap_or_default()
}

/// Focus an element we captured with `onmounted`, if it is still around.
fn focus(el: Signal<Option<std::rc::Rc<MountedData>>>) {
    if let Some(el) = el() {
        spawn(async move {
            let _ = el.set_focus(true).await;
        });
    }
}

#[component]
pub fn ActivityView(
    store: Signal<Option<SharedStore>>,
    filter: Signal<TxnQuery>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
    scroll: Signal<f64>,
) -> Element {
    let mut filter = filter;
    let status = status;
    let mut cursor = use_signal(|| 0usize);
    // Set right before a cursor move that should scroll the row into view, so mounting alone never scrolls.
    let mut follow = use_signal(|| false);
    let mut selected = use_signal(Vec::<String>::new);
    let mut typed = use_signal(String::new);
    let mut expanded = use_signal(|| None::<String>);
    let mut armed = use_signal(|| false);
    let mut list_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut search_el = use_signal(|| None::<std::rc::Rc<MountedData>>);
    // Row elements by id, so a click can measure where its row sits before the editor moves things.
    let mut row_els =
        use_signal(std::collections::HashMap::<String, std::rc::Rc<MountedData>>::new);
    // (row id, viewport top) of a row clicked open, cleared once the editor has settled it in place.
    let mut anchor = use_signal(|| None::<(String, f64)>);
    let _tick = nonce();

    let (txns, cats, accounts, months, ai_ready, debug) = match store() {
        Some(s) => {
            let g = s.lock().unwrap();
            (
                g.query_transactions(&filter()).unwrap_or_default(),
                g.list_categories().unwrap_or_default(),
                g.list_accounts().unwrap_or_default(),
                g.list_months().unwrap_or_default(),
                g.ai_settings().map(|a| a.is_configured()).unwrap_or(false),
                g.debug_enabled().unwrap_or(false),
            )
        }
        None => {
            return rsx! {
                p { "Locked" }
            }
        }
    };

    if !txns.is_empty() && cursor() >= txns.len() {
        cursor.set(txns.len() - 1);
    }

    let f = filter();
    let typed_now = typed();
    let hint = match_category_typeahead(&typed_now, &cats).map(|c| c.label());
    let filtered = f != default_filter() && f != TxnQuery::default();
    let n_sel = selected.read().len();
    // The header checkbox reads as checked only when every row on screen is selected.
    let all_selected = !txns.is_empty()
        && txns
            .iter()
            .all(|t| selected.read().iter().any(|id| id == &t.id));
    let out_total: i64 = txns
        .iter()
        .filter(|t| !t.excluded && !t.is_transfer && t.amount_cents < 0)
        .map(|t| -t.amount_cents)
        .sum();
    let in_total: i64 = txns
        .iter()
        .filter(|t| !t.excluded && !t.is_transfer && t.amount_cents > 0)
        .map(|t| t.amount_cents)
        .sum();

    let groups = group_rows(&txns);
    // Rows on screen the AI could fill in, behind the "Categorize with AI" button. Rows whose
    // last attempt failed are left to "Ask AI" on the row itself or Setup → AI.
    let ai_ids: Vec<String> = txns
        .iter()
        .filter(|t| ai_candidate(t) && t.ai_failed_at.is_none())
        .map(|t| t.id.clone())
        .collect();
    let ai_busy = syncing.read().is_some();

    let set_excluded = {
        let txns = txns.clone();
        move |ids: Vec<String>| {
            if ids.is_empty() {
                return;
            }
            // Toggle: include again only if every target is already excluded.
            let all_ex = ids
                .iter()
                .all(|id| txns.iter().any(|t| &t.id == id && t.excluded));
            if let Some(s) = store() {
                let r = s.lock().unwrap().patch_transactions(
                    &ids,
                    &TxnPatch {
                        excluded: Some(!all_ex),
                        ..Default::default()
                    },
                );
                if let Err(e) = r {
                    push_status(status, e.as_user_message());
                }
            }
            selected.set(Vec::new());
            super::bump(nonce);
        }
    };
    // Bulk "Mark as…" for the selection: a flag patch keyed by the dropdown's value.
    let mut mark_selected = move |ids: Vec<String>, what: String| {
        let Some(patch) = bulk_mark_patch(&what) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        if let Some(s) = store() {
            if let Err(e) = s.lock().unwrap().patch_transactions(&ids, &patch) {
                push_status(status, e.as_user_message());
            }
        }
        selected.set(Vec::new());
        super::bump(nonce);
    };
    let mut delete_rows = move |ids: Vec<String>| {
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        if let Some(s) = store() {
            let st = s.lock().unwrap();
            for id in &ids {
                if let Err(e) = st.tombstone_and_delete(id) {
                    push_status(status, e.as_user_message());
                    break;
                }
            }
        }
        push_status(status, format!("Deleted {n}."));
        selected.set(Vec::new());
        armed.set(false);
        super::bump(nonce);
    };
    let mut set_filter = move |f: TxnQuery| {
        filter.set(f);
        follow.set(true);
        cursor.set(0);
        selected.set(Vec::new());
        armed.set(false);
        focus(list_el);
    };

    // After a clicked row's editor opens, scroll by however far that row moved so it stays put on screen.
    let settle = move |_| {
        let Some((id, before)) = anchor.peek().clone() else {
            return;
        };
        let Some(el) = row_els.peek().get(&id).cloned() else {
            anchor.set(None);
            return;
        };
        spawn(async move {
            if let Ok(rect) = el.get_client_rect().await {
                let delta = rect.origin.y - before;
                if delta.abs() >= 1.0 {
                    document::eval(&format!("window.scrollBy(0, {delta});"));
                }
            }
            anchor.set(None);
        });
    };

    rsx! {
        div {
            class: "activity",
            tabindex: "0",
            autofocus: true,
            onmounted: move |e| {
                list_el.set(Some(e.data()));
                // Pick up where the user left off, or the top when they never scrolled here.
                let y = *scroll.peek();
                document::eval(&format!("window.scrollTo(0, {y});"));
            },
            onkeydown: {
                let txns = txns.clone();
                let cats = cats.clone();
                let mut set_excluded = set_excluded.clone();
                move |evt| {
                    let key = evt.key();
                    let mods = evt.modifiers();
                    let cmd = mods.meta() || mods.ctrl();
                    let n = txns.len();
                    let i = cursor();
                    let ids = targets(&selected(), &txns, i);
                    let was_armed = armed();
                    // Any key other than a repeat of the delete chord disarms it.
                    if was_armed && !(cmd && (key == Key::Backspace || key == Key::Delete)) {
                        armed.set(false);
                    }
                    match key {
                        Key::ArrowDown => {
                            evt.prevent_default();
                            if n > 0 {
                                follow.set(true);
                                cursor.set((i + 1).min(n - 1));
                            }
                        }
                        Key::ArrowUp => {
                            evt.prevent_default();
                            follow.set(true);
                            cursor.set(i.saturating_sub(1));
                        }
                        Key::Character(c) if c == " " && typed().is_empty() => {
                            evt.prevent_default();
                            if let Some(t) = txns.get(i) {
                                let mut sel = selected();
                                match sel.iter().position(|id| id == &t.id) {
                                    Some(pos) => {
                                        sel.remove(pos);
                                    }
                                    None => sel.push(t.id.clone()),
                                }
                                selected.set(sel);
                            }
                        }
                        Key::Character(c) if c == "/" && typed().is_empty() => {
                            evt.prevent_default();
                            focus(search_el);
                        }
                        Key::Character(c) if c == "e" && cmd => {
                            evt.prevent_default();
                            set_excluded(ids);
                        }
                        Key::Backspace | Key::Delete if cmd => {
                            evt.prevent_default();
                            if ids.is_empty() {
                                return;
                            }
                            if was_armed {
                                delete_rows(ids);
                            } else {
                                armed.set(true);
                            }
                        }
                        Key::Backspace => {
                            let mut t = typed();
                            t.pop();
                            typed.set(t);
                        }
                        Key::Escape => {
                            if was_armed {} else if !typed().is_empty() {
                                typed.set(String::new());
                            } else if expanded().is_some() {
                                expanded.set(None);
                            } else if !selected().is_empty() {
                                selected.set(Vec::new());
                            }
                        }
                        Key::Enter => {
                            let name = typed();
                            if name.trim().is_empty() {
                                if let Some(t) = txns.get(i) {
                                    let open = expanded().as_deref() == Some(t.id.as_str());
                                    expanded.set(if open { None } else { Some(t.id.clone()) });
                                }
                                return;
                            }
                            match match_category_typeahead(&name, &cats) {
                                Some(cat) => {
                                    apply_txn_category(
                                        store,
                                        status,
                                        nonce,
                                        &ids,
                                        Some(cat.id.clone()),
                                    );
                                    selected.set(Vec::new());
                                    typed.set(String::new());
                                }
                                None => {
                                    push_status(
                                        status,
                                        format!("No single category matches '{name}'."),
                                    )
                                }
                            }
                        }
                        Key::Character(c) if c.chars().count() == 1 && !cmd => {
                            let mut t = typed();
                            t.push_str(&c);
                            typed.set(t);
                        }
                        _ => {}
                    }
                }
            },

            div { class: "filters",
                div { class: "scope", role: "group", aria_label: "Scope",
                    button {
                        class: if f.uncategorized_only { "chip on" } else { "chip" },
                        onclick: move |_| set_filter(TxnQuery {
                            uncategorized_only: true,
                            ai_only: false,
                            excluded_only: false,
                            category_id: None,
                            ..filter()
                        }),
                        "Uncategorized"
                    }
                    button {
                        class: if !f.uncategorized_only && !f.ai_only && !f.excluded_only && f.category_id.is_none() { "chip on" } else { "chip" },
                        onclick: move |_| set_filter(TxnQuery {
                            uncategorized_only: false,
                            ai_only: false,
                            excluded_only: false,
                            category_id: None,
                            ..filter()
                        }),
                        "All"
                    }
                    button {
                        class: if f.ai_only { "chip on" } else { "chip" },
                        title: "Rows the AI categorized, for review",
                        onclick: move |_| set_filter(TxnQuery {
                            uncategorized_only: false,
                            ai_only: true,
                            excluded_only: false,
                            category_id: None,
                            ..filter()
                        }),
                        "AI"
                    }
                    button {
                        class: if f.excluded_only { "chip on" } else { "chip" },
                        title: "Rows you excluded, so they can be included again",
                        onclick: move |_| set_filter(TxnQuery {
                            uncategorized_only: false,
                            ai_only: false,
                            excluded_only: true,
                            category_id: None,
                            ..filter()
                        }),
                        "Excluded"
                    }
                }
                input {
                    class: "search",
                    r#type: "search",
                    placeholder: "Search payee, notes, or amount  ( / )",
                    aria_label: "Search transactions",
                    value: "{f.search}",
                    onmounted: move |e| search_el.set(Some(e.data())),
                    oninput: move |e| {
                        filter.write().search = e.value();
                        follow.set(true);
                        cursor.set(0);
                    },
                    onkeydown: move |evt| {
                        evt.stop_propagation();
                        match evt.key() {
                            Key::Escape => {
                                filter.write().search.clear();
                                focus(list_el);
                            }
                            Key::Enter | Key::ArrowDown => focus(list_el),
                            _ => {}
                        }
                    },
                }
                if accounts.len() > 1 {
                    select {
                        aria_label: "Account",
                        value: f.account_id.clone().unwrap_or_default(),
                        onkeydown: move |e| e.stop_propagation(),
                        onchange: move |e| {
                            let v = e.value();
                            set_filter(TxnQuery {
                                account_id: if v.is_empty() { None } else { Some(v) },
                                ..filter()
                            });
                        },
                        option { value: "", "All accounts" }
                        for a in accounts.iter() {
                            option {
                                value: "{a.id}",
                                selected: f.account_id.as_deref() == Some(a.id.as_str()),
                                if a.hidden {
                                    "{a.label()} (hidden)"
                                } else {
                                    "{a.label()}"
                                }
                            }
                        }
                    }
                }
                if !cats.is_empty() {
                    select {
                        aria_label: "Category filter",
                        value: f.category_id.clone().unwrap_or_default(),
                        onkeydown: move |e| e.stop_propagation(),
                        onchange: move |e| {
                            let v = e.value();
                            let cat = if v.is_empty() { None } else { Some(v) };
                            set_filter(TxnQuery {
                                uncategorized_only: false,
                                category_id: cat,
                                ..filter()
                            });
                        },
                        option { value: "", "Any category" }
                        for c in cats.iter() {
                            option {
                                value: "{c.id}",
                                selected: f.category_id.as_deref() == Some(c.id.as_str()),
                                "{c.label()}"
                            }
                        }
                    }
                }
                if !months.is_empty() {
                    select {
                        aria_label: "Month",
                        value: f.month.map(|(y, m)| format!("{y}-{m}")).unwrap_or_default(),
                        onkeydown: move |e| e.stop_propagation(),
                        onchange: move |e| {
                            let v = e.value();
                            let month = v
                                .split_once('-')
                                .and_then(|(y, m)| Some((y.parse().ok()?, m.parse().ok()?)));
                            set_filter(TxnQuery { month, ..filter() });
                        },
                        option { value: "", "Any month" }
                        for (y, m) in months.iter() {
                            option {
                                value: "{y}-{m}",
                                selected: f.month == Some((*y, *m)),
                                "{month_label(*y, *m)}"
                            }
                        }
                    }
                }
                if filtered {
                    button {
                        class: "ghost small",
                        onclick: move |_| set_filter(default_filter()),
                        "Reset"
                    }
                }
            }

            div { class: "list-meta",
                span { class: "count",
                    if !txns.is_empty() {
                        input {
                            r#type: "checkbox",
                            class: "pick pick-all",
                            aria_label: if all_selected { "Deselect all" } else { "Select all" },
                            title: if all_selected { "Deselect all" } else { "Select all shown" },
                            checked: all_selected,
                            onchange: {
                                let ids: Vec<String> = txns.iter().map(|t| t.id.clone()).collect();
                                move |_| {
                                    if all_selected {
                                        selected.set(Vec::new());
                                    } else {
                                        selected.set(ids.clone());
                                    }
                                    armed.set(false);
                                }
                            },
                        }
                    }
                    "{txns.len()} {plural(txns.len(), \"transaction\", \"transactions\")}"
                    if out_total > 0 {
                        span { class: "sum", " · out ${format_cents(out_total)}" }
                    }
                    if in_total > 0 {
                        span { class: "sum in", " · in ${format_cents(in_total)}" }
                    }
                }
                if f.uncategorized_only && ai_ready && !ai_ids.is_empty() {
                    button {
                        class: "ghost small",
                        disabled: ai_busy,
                        title: "Send the uncategorized rows shown here to the AI",
                        onclick: {
                            let ids = ai_ids.clone();
                            move |_| start_ai_run(store, Some(ids.clone()), syncing, status, nonce)
                        },
                        if ai_busy {
                            "Asking…"
                        } else {
                            "Categorize {ai_ids.len()} with AI"
                        }
                    }
                }
                if armed() {
                    span { class: "arm",
                        "Press ⌘⌫ again to delete {targets(&selected(), &txns, cursor()).len()}. Esc cancels."
                    }
                } else if !typed_now.is_empty() {
                    span { class: "typeahead",
                        span { class: "k", "Category: " }
                        strong { "{typed_now}" }
                        match hint.as_ref() {
                            Some(name) if !name.eq_ignore_ascii_case(typed_now.trim()) => rsx! {
                                span { class: "guess", " → {name}" }
                                span { class: "k", "  Enter" }
                            },
                            Some(_) => rsx! {
                                span { class: "k", "  Enter" }
                            },
                            None => rsx! {
                                span { class: "guess none", "  no single match" }
                            },
                        }
                    }
                } else {
                    span { class: "keys",
                        kbd { "↑↓" }
                        " move · type a category, "
                        kbd { "Enter" }
                        " · "
                        kbd { "Space" }
                        " select · "
                        kbd { "⌘E" }
                        " exclude · "
                        kbd { "⌘⌫" }
                        " delete · "
                        kbd { "/" }
                        " search"
                    }
                }
            }

            if n_sel > 0 {
                div { class: "bulk",
                    strong { "{n_sel} selected" }
                    if !cats.is_empty() {
                        CategoryPicker {
                            store,
                            status,
                            nonce,
                            ids: selected(),
                            cats: cats.clone(),
                            current_id: None,
                            class: "".to_string(),
                            prompt: Some("Set category…".into()),
                            selection: Some(selected),
                        }
                    }
                    select {
                        aria_label: "Mark selected as",
                        value: "",
                        onchange: move |e| mark_selected(selected(), e.value()),
                        option { value: "", "Mark as…" }
                        option { value: "transfer", "transfer" }
                        option { value: "card_payment", "credit card payment" }
                        option { value: "loan_payment", "loan payment" }
                        option { value: "income", "income / paycheck" }
                        option { value: "not_transfer", "not a transfer" }
                        option { value: "not_income", "not income" }
                    }
                    button {
                        onclick: {
                            let mut f = set_excluded.clone();
                            move |_| f(selected())
                        },
                        "Exclude"
                    }
                    button {
                        class: if armed() { "danger on" } else { "danger" },
                        onclick: move |_| if armed() { delete_rows(selected()) } else { armed.set(true) },
                        if armed() {
                            "Confirm delete"
                        } else {
                            "Delete"
                        }
                    }
                    button {
                        class: "ghost",
                        onclick: move |_| {
                            selected.set(Vec::new());
                            armed.set(false);
                        },
                        "Clear"
                    }
                }
            }

            if txns.is_empty() {
                div { class: "empty-state",
                    if f.uncategorized_only && !filtered {
                        p { "Everything is categorized." }
                        p { class: "hint",
                            "Sync in Setup to pull new transactions, or switch to All."
                        }
                    } else if f.ai_only
                        && f
                            == (TxnQuery {
                                ai_only: true,
                                ..Default::default()
                            })
                    {
                        p { "Nothing categorized by AI yet." }
                        p { class: "hint", "Set up a provider in Setup → AI, then run it." }
                    } else if f.excluded_only
                        && f
                            == (TxnQuery {
                                excluded_only: true,
                                ..Default::default()
                            })
                    {
                        p { "Nothing is excluded." }
                        p { class: "hint",
                            "Exclude a row from its editor or with ⌘E, and it shows up here."
                        }
                    } else if filtered {
                        p { "Nothing matches these filters." }
                        button {
                            class: "ghost",
                            onclick: move |_| set_filter(default_filter()),
                            "Reset filters"
                        }
                    } else {
                        p { "No transactions yet." }
                        p { class: "hint", "Connect a bank in Setup and sync." }
                    }
                }
            }

            ul { class: "txns", role: "list",
                for (label, rows) in groups {
                    li { class: "group", "{label}" }
                    for (i, t) in rows {
                        li {
                            key: "{t.id}",
                            class: format_args!(
                                "txn{}{}{}",
                                if i == cursor() { " cur" } else { "" },
                                if selected.read().iter().any(|id| id == &t.id) { " sel" } else { "" },
                                if t.excluded { " ex" } else { "" },
                            ),
                            onmounted: {
                                let id = t.id.clone();
                                move |e| {
                                    row_els.write().insert(id.clone(), e.data());
                                }
                            },
                            div {
                                class: "txn-main",
                                onclick: {
                                    let id = t.id.clone();
                                    move |_| {
                                        let id = id.clone();
                                        let open = expanded.read().as_deref() == Some(id.as_str());
                                        let el = if open { None } else { row_els.peek().get(&id).cloned() };
                                        spawn(async move {
                                            // Opening can collapse an editor above; note where this row is first.
                                            if let Some(el) = el {
                                                if let Ok(rect) = el.get_client_rect().await {
                                                    anchor.set(Some((id.clone(), rect.origin.y)));
                                                }
                                            }
                                            cursor.set(i);
                                            expanded.set(if open { None } else { Some(id) });
                                        });
                                    }
                                },
                                if i == cursor() {
                                    // Only the cursor row has this, so moving the cursor scrolls it into view.
                                    span {
                                        class: "cur-mark",
                                        onmounted: move |e| {
                                            // Only a keyboard or filter move asks for this; mounting alone
                                            // (fresh screen, click, resize) leaves the scroll position alone.
                                            if !*follow.peek() {
                                                return;
                                            }
                                            follow.set(false);
                                            // A clicked-open row is held in place by `settle` instead.
                                            if anchor.peek().is_some() {
                                                return;
                                            }
                                            spawn(async move {
                                                let _ = e
                                                    .data()
                                                    .scroll_to_with_options(ScrollToOptions {
                                                        behavior: ScrollBehavior::Instant,
                                                        vertical: ScrollLogicalPosition::Nearest,
                                                        horizontal: ScrollLogicalPosition::Nearest,
                                                    })
                                                    .await;
                                            });
                                        },
                                    }
                                }
                                input {
                                    r#type: "checkbox",
                                    class: "pick",
                                    aria_label: "Select",
                                    checked: selected.read().iter().any(|id| id == &t.id),
                                    onclick: move |e| e.stop_propagation(),
                                    onchange: {
                                        let id = t.id.clone();
                                        move |_| {
                                            let mut sel = selected();
                                            match sel.iter().position(|x| x == &id) {
                                                Some(pos) => {
                                                    sel.remove(pos);
                                                }
                                                None => sel.push(id.clone()),
                                            }
                                            selected.set(sel);
                                        }
                                    },
                                }
                                span { class: "date",
                                    if t.posted_at > 0 {
                                        "{fmt_day(t.posted_at)}"
                                    }
                                }
                                span { class: "payee",
                                    span { class: "payee-name", "{t.payee}" }
                                    if t.pending {
                                        span { class: "tag", "pending" }
                                    }
                                    if let Some(p) = t.payment {
                                        span { class: "tag", "{p.label()}" }
                                    } else if t.is_transfer {
                                        span { class: "tag", "transfer" }
                                    }
                                    if t.is_income {
                                        span { class: "tag", "income" }
                                    }
                                    if t.has_splits {
                                        span { class: "tag", "split" }
                                    }
                                    if t.categorized_by.as_deref() == Some("ai") {
                                        span {
                                            class: "tag",
                                            title: "Category picked by AI",
                                            "ai"
                                        }
                                    }
                                    if t.excluded {
                                        span { class: "tag", "excluded" }
                                    }
                                    if let Some(n) = t.notes.as_deref().filter(|n| !n.trim().is_empty()) {
                                        span { class: "note", title: "{n}", "{n}" }
                                    }
                                }
                                span { class: "acct", "{t.account_name}" }
                                if cats.is_empty() {
                                    span { class: "cat", "-" }
                                } else {
                                    CategoryPicker {
                                        store,
                                        status,
                                        nonce,
                                        ids: vec![t.id.clone()],
                                        cats: cats.clone(),
                                        current_id: t.category_id.clone(),
                                        class: "cat".to_string(),
                                        prompt: None,
                                        selection: None,
                                    }
                                }
                                span { class: if t.amount_cents < 0 { "amt out" } else { "amt in" },
                                    if t.amount_cents < 0 {
                                        "−"
                                    } else {
                                        "+"
                                    }
                                    "${format_cents(t.amount_cents.abs())}"
                                }
                            }
                            if expanded.read().as_deref() == Some(t.id.as_str()) {
                                RowEditor {
                                    store,
                                    txn: t.clone(),
                                    nonce,
                                    status,
                                    syncing,
                                    ai_ready,
                                    debug,
                                    cats: cats.clone(),
                                    expanded,
                                    on_open: settle,
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Inline editor for one expanded transaction.
#[component]
fn RowEditor(
    store: Signal<Option<SharedStore>>,
    txn: Txn,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    syncing: Signal<Option<String>>,
    /// An AI provider and key are saved, so the "Ask AI" action can show.
    ai_ready: bool,
    /// Setup → Debug is on, so the raw data and AI trace panels can show.
    debug: bool,
    cats: Vec<Category>,
    expanded: Signal<Option<String>>,
    /// Fired once the editor is in the DOM, so the parent can keep the row from shifting.
    on_open: EventHandler<()>,
) -> Element {
    let mut expanded = expanded;
    let mut payee = use_signal(|| txn.payee.clone());
    let mut notes = use_signal(|| txn.notes.clone().unwrap_or_default());
    let mut amount = use_signal(|| format_cents(txn.amount_cents));
    let mut date = use_signal(|| fmt_iso(txn.posted_at));
    let mut show_split = use_signal(|| false);
    let mut show_rule = use_signal(|| false);
    let mut show_raw = use_signal(|| false);
    let mut show_ai = use_signal(|| false);
    // Rule form, prefilled from this row: exact payee, and whatever the row already is.
    let rule_patterns: Signal<Vec<String>> = use_signal(|| vec![txn.payee.clone()]);
    let mut rule_target = use_signal(|| default_rule_target(&txn));
    // Id of an existing rule the pattern is added to; empty means a new rule.
    let mut rule_existing = use_signal(String::new);
    let mut armed = use_signal(|| false);
    let mut split_a = use_signal(String::new);
    let mut split_b = use_signal(String::new);
    let mut split_cat_a = use_signal(|| cats.first().map(|c| c.id.clone()).unwrap_or_default());
    let mut split_cat_b = use_signal(|| {
        cats.get(1)
            .or(cats.first())
            .map(|c| c.id.clone())
            .unwrap_or_default()
    });
    let id = txn.id.clone();

    // Rows the typed pattern would apply to, and the rules it could join, only while the rule
    // form is open. The preview covers just the typed pattern, so when adding to an existing
    // rule it shows what the new alternative brings in.
    let (rule_preview, rule_suggestion, existing_rules) = if show_rule() {
        let pattern = join_patterns(&rule_patterns.read());
        let rows = store()
            .and_then(|s| s.lock().ok().and_then(|g| g.preview_rule(&pattern).ok()))
            .unwrap_or_default();
        let suggestion = store()
            .and_then(|s| {
                s.lock()
                    .ok()
                    .and_then(|g| g.suggest_contains_pattern(&pattern).ok())
            })
            .flatten();
        let rules = store()
            .and_then(|s| s.lock().ok().and_then(|g| g.list_rules().ok()))
            .unwrap_or_default();
        (rows, suggestion, rules)
    } else {
        (Vec::new(), None, Vec::new())
    };
    let adding_to_existing = !rule_existing.read().is_empty();

    let mut add_rule = move |_| {
        let pattern = join_patterns(&rule_patterns.read());
        let existing = rule_existing.read().clone();
        if !existing.is_empty() {
            if pattern.is_empty() {
                push_status(status, "Need a pattern.");
                return;
            }
            if let Some(s) = store() {
                let r = s.lock().unwrap().add_rule_pattern(&existing, &pattern);
                push_status(status, rule_extended_message(r));
            }
            show_rule.set(false);
            super::bump(nonce);
            return;
        }
        let (action, category_id) = parse_rule_target(&rule_target.read());
        if pattern.is_empty() || (action == RuleAction::Category && category_id.is_none()) {
            push_status(status, "Need a pattern and an action.");
            return;
        }
        if let Some(s) = store() {
            let st = s.lock().unwrap();
            let priority = st.list_rules().map(|r| r.len() as i64 + 1).unwrap_or(1);
            let r = st.create_rule(&Rule::pattern(&pattern, action, category_id, priority));
            push_status(status, rule_added_message(r));
        }
        show_rule.set(false);
        super::bump(nonce);
    };

    // Apply a patch to this row. A non-empty `done` is toasted and logged with an Undo that
    // puts the row back as it is now.
    let before = txn.clone();
    let patch = move |p: TxnPatch, done: &str| {
        if let Some(s) = store() {
            let undo = Undo {
                ids: vec![id.clone()],
                patch: p.reverse_for(&before),
            };
            match s.lock().unwrap().patch_transactions(&[id.clone()], &p) {
                Ok(_) => {
                    if !done.is_empty() {
                        push_undoable(status, done, undo);
                    }
                }
                Err(e) => push_status(status, e.as_user_message()),
            }
        }
        super::bump(nonce);
    };

    // The row's current flag; changing the select applies at once, so nothing is held here.
    let mark = current_mark(&txn);

    rsx! {
        div {
            class: "editor",
            onmounted: move |_| on_open.call(()),
            onclick: move |evt| evt.stop_propagation(),
            onkeydown: move |evt| {
                evt.stop_propagation();
                if evt.key() == Key::Escape {
                    expanded.set(None);
                }
            },
            div { class: "editor-head",
                span { class: "hint", "{txn.account_name}" }
                button {
                    class: "ghost small",
                    aria_label: "Close editor",
                    onclick: move |_| expanded.set(None),
                    "Close"
                }
            }
            div { class: "editor-fields",
                if !cats.is_empty() {
                    label {
                        "Category"
                        div { class: "field-row",
                            CategoryPicker {
                                store,
                                status,
                                nonce,
                                ids: vec![txn.id.clone()],
                                cats: cats.clone(),
                                current_id: txn.category_id.clone(),
                                class: "".to_string(),
                                prompt: None,
                                selection: None,
                            }
                            if ai_ready && ai_candidate(&txn) {
                                button {
                                    class: "ghost small",
                                    disabled: syncing.read().is_some(),
                                    title: "Send just this row to the AI",
                                    onclick: {
                                        let id = txn.id.clone();
                                        move |_| start_ai_run(store, Some(vec![id.clone()]), syncing, status, nonce)
                                    },
                                    if syncing.read().as_deref() == Some(super::AI_BUSY) {
                                        "Asking…"
                                    } else {
                                        "Ask AI"
                                    }
                                }
                            }
                        }
                        if ai_ready && ai_candidate(&txn) && txn.ai_failed_at.is_some() {
                            span { class: "hint",
                                "The AI failed on this row earlier. It is skipped after syncs until you ask again."
                            }
                        }
                    }
                }
                // Each field saves when it loses focus or takes Enter, as its own undoable change.
                label {
                    "Payee"
                    input {
                        value: "{payee}",
                        oninput: move |e| payee.set(e.value()),
                        onchange: {
                            let patch = patch.clone();
                            let was = txn.payee.clone();
                            move |_| {
                                let v = payee.read().trim().to_string();
                                if v.is_empty() {
                                    push_status(status, "Payee can't be blank.");
                                    payee.set(was.clone());
                                } else if v != was {
                                    patch(
                                        TxnPatch {
                                            payee: Some(v.clone()),
                                            ..Default::default()
                                        },
                                        &format!("Renamed {was} to {v}."),
                                    );
                                }
                            }
                        },
                    }
                }
                label {
                    "Notes"
                    input {
                        value: "{notes}",
                        placeholder: "optional",
                        oninput: move |e| notes.set(e.value()),
                        onchange: {
                            let patch = patch.clone();
                            let was = txn.notes.clone();
                            let who = txn.payee.clone();
                            move |_| {
                                let v = notes.read().trim().to_string();
                                let v = if v.is_empty() { None } else { Some(v) };
                                if v != was {
                                    let done = if v.is_some() {
                                        format!("Changed notes on {who}.")
                                    } else {
                                        format!("Cleared notes on {who}.")
                                    };
                                    patch(
                                        TxnPatch {
                                            notes: Some(v),
                                            ..Default::default()
                                        },
                                        &done,
                                    );
                                }
                            }
                        },
                    }
                }
                label {
                    "Date"
                    input {
                        r#type: "date",
                        value: "{date}",
                        oninput: move |e| date.set(e.value()),
                        onchange: {
                            let patch = patch.clone();
                            let was = txn.posted_at;
                            let who = txn.payee.clone();
                            move |_| {
                                let typed = date.read().clone();
                                let Some(posted) = parse_iso(&typed) else {
                                    push_status(status, "Date must be YYYY-MM-DD.");
                                    date.set(fmt_iso(was));
                                    return;
                                };
                                if posted != was {
                                    patch(
                                        TxnPatch {
                                            posted_at: Some(posted),
                                            ..Default::default()
                                        },
                                        &format!("Moved {who} to {typed}."),
                                    );
                                }
                            }
                        },
                    }
                }
                label {
                    "Amount"
                    input {
                        class: "num",
                        value: "{amount}",
                        oninput: move |e| amount.set(e.value()),
                        onchange: {
                            let patch = patch.clone();
                            let was = txn.amount_cents;
                            let who = txn.payee.clone();
                            move |_| {
                                let typed = amount.read().clone();
                                let cents = match myphin::money::parse_cents(&typed) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        push_status(status, e.as_user_message());
                                        amount.set(format_cents(was));
                                        return;
                                    }
                                };
                                if cents != was {
                                    patch(
                                        TxnPatch {
                                            amount_cents: Some(cents),
                                            ..Default::default()
                                        },
                                        &format!("Changed amount of {who} to {}.", format_cents(cents)),
                                    );
                                }
                            }
                        },
                    }
                }
            }
            div { class: "row actions",
                select {
                    aria_label: "Mark as",
                    value: "{mark}",
                    onchange: {
                        let patch = patch.clone();
                        let payee = txn.payee.clone();
                        move |e| {
                            let to = e.value();
                            if let Some(p) = mark_patch(mark, &to) {
                                patch(p, &mark_message(&payee, &to));
                            }
                        }
                    },
                    option { value: "", selected: mark.is_empty(), "Mark as…" }
                    option { value: "transfer", selected: mark == "transfer", "transfer" }
                    option {
                        value: "card_payment",
                        selected: mark == "card_payment",
                        "credit card payment"
                    }
                    option {
                        value: "loan_payment",
                        selected: mark == "loan_payment",
                        "loan payment"
                    }
                    option { value: "income", selected: mark == "income", "income / paycheck" }
                    option { value: "exclude", selected: mark == "exclude", "excluded" }
                }
                if !cats.is_empty() && !txn.has_splits {
                    button { class: "ghost", onclick: move |_| show_split.toggle(),
                        if show_split() {
                            "Hide split"
                        } else {
                            "Split…"
                        }
                    }
                }
                button { class: "ghost", onclick: move |_| show_rule.toggle(),
                    if show_rule() {
                        "Hide rule"
                    } else {
                        "Create rule…"
                    }
                }
                if debug {
                    button { class: "ghost", onclick: move |_| show_raw.toggle(),
                        if show_raw() {
                            "Hide raw data"
                        } else {
                            "Raw data"
                        }
                    }
                    button { class: "ghost", onclick: move |_| show_ai.toggle(),
                        if show_ai() {
                            "Hide AI trace"
                        } else {
                            "AI trace"
                        }
                    }
                }
                span { class: "spacer" }
                button {
                    class: if armed() { "danger on" } else { "danger" },
                    onclick: {
                        let id = txn.id.clone();
                        move |_| {
                            if !armed() {
                                armed.set(true);
                                return;
                            }
                            if let Some(s) = store() {
                                if let Err(e) = s.lock().unwrap().tombstone_and_delete(&id) {
                                    push_status(status, e.as_user_message());
                                }
                            }
                            push_status(status, "Deleted.");
                            expanded.set(None);
                            super::bump(nonce);
                        }
                    },
                    if armed() {
                        "Confirm delete"
                    } else {
                        "Delete"
                    }
                }
                if armed() {
                    button {
                        class: "ghost small",
                        onclick: move |_| armed.set(false),
                        "Cancel"
                    }
                }
            }
            if debug && show_raw() {
                RawDataPanel { store, txn_id: txn.id.clone() }
            }
            if debug && show_ai() {
                AiTracePanel { store, txn_id: txn.id.clone(), nonce }
            }
            if show_rule() {
                div { class: "splits rule-panel",
                    p {
                        "Every transaction matching any of the patterns gets this action, now and after each sync. "
                        code { "*" }
                        " stands for anything."
                    }
                    if !existing_rules.is_empty() {
                        div { class: "row rule-form",
                            span { class: "hint", "Add to" }
                            select {
                                aria_label: "Rule to add to",
                                value: "{rule_existing}",
                                onchange: move |e| rule_existing.set(e.value()),
                                option { value: "", selected: !adding_to_existing, "a new rule" }
                                for r in existing_rules.iter() {
                                    option {
                                        value: "{r.id}",
                                        selected: rule_existing() == r.id,
                                        "{rule_summary(r, &cats)}"
                                    }
                                }
                            }
                        }
                    }
                    p { class: "hint", "If description matches" }
                    PatternList {
                        patterns: rule_patterns,
                        on_submit: move |_| add_rule(()),
                        on_cancel: move |_| show_rule.set(false),
                    }
                    div { class: "row rule-form",
                        if !adding_to_existing {
                            span { class: "hint", "then" }
                            select {
                                aria_label: "Rule action",
                                value: "{rule_target}",
                                onchange: move |e| rule_target.set(e.value()),
                                option { value: "", "pick an action" }
                                optgroup { label: "Mark as",
                                    option {
                                        value: "transfer",
                                        selected: rule_target() == "transfer",
                                        "transfer"
                                    }
                                    option {
                                        value: "card_payment",
                                        selected: rule_target() == "card_payment",
                                        "credit card payment"
                                    }
                                    option {
                                        value: "loan_payment",
                                        selected: rule_target() == "loan_payment",
                                        "loan payment"
                                    }
                                    option {
                                        value: "income",
                                        selected: rule_target() == "income",
                                        "income / paycheck"
                                    }
                                    option {
                                        value: "exclude",
                                        selected: rule_target() == "exclude",
                                        "excluded"
                                    }
                                }
                                if !cats.is_empty() {
                                    optgroup { label: "Categorize as",
                                        for c in cats.iter() {
                                            option {
                                                value: "cat:{c.id}",
                                                selected: rule_target() == format!("cat:{}", c.id),
                                                "{c.label()}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        button { class: "primary", onclick: move |_| add_rule(()),
                            if adding_to_existing {
                                "Add to rule"
                            } else {
                                "Add rule"
                            }
                        }
                    }
                    RulePreview { rows: rule_preview, suggestion: rule_suggestion }
                }
            }
            if show_split() && !cats.is_empty() {
                div { class: "splits",
                    p {
                        "Split into two categories. The two amounts must add up to ${format_cents(txn.amount_cents)}."
                    }
                    div { class: "split-row",
                        select {
                            aria_label: "First split category",
                            onchange: move |e| split_cat_a.set(e.value()),
                            for c in cats.iter() {
                                option { value: "{c.id}", "{c.label()}" }
                            }
                        }
                        input {
                            class: "num",
                            placeholder: "-10.00",
                            value: "{split_a}",
                            oninput: move |e| split_a.set(e.value()),
                        }
                    }
                    div { class: "split-row",
                        select {
                            aria_label: "Second split category",
                            onchange: move |e| split_cat_b.set(e.value()),
                            for (i, c) in cats.iter().enumerate() {
                                option {
                                    value: "{c.id}",
                                    selected: i == 1.min(cats.len() - 1),
                                    "{c.label()}"
                                }
                            }
                        }
                        input {
                            class: "num",
                            placeholder: "-10.00",
                            value: "{split_b}",
                            oninput: move |e| split_b.set(e.value()),
                        }
                    }
                    div { class: "row",
                        button {
                            class: "primary",
                            onclick: {
                                let id = txn.id.clone();
                                let total = txn.amount_cents;
                                move |_| {
                                    let a = myphin::money::parse_cents(split_a.read().as_str());
                                    let b = myphin::money::parse_cents(split_b.read().as_str());
                                    let (a, b) = match (a, b) {
                                        (Ok(a), Ok(b)) => (a, b),
                                        _ => {
                                            push_status(status, "Enter two amounts like -10.00.");
                                            return;
                                        }
                                    };
                                    if a + b != total {
                                        push_status(
                                            status,
                                            format!(
                                                "Splits add up to ${}, not ${}.",
                                                format_cents(a + b),
                                                format_cents(total),
                                            ),
                                        );
                                        return;
                                    }
                                    if let Some(s) = store() {
                                        let r = s
                                            .lock()
                                            .unwrap()
                                            .split_transaction(
                                                &id,
                                                &[(Some(split_cat_a()), a), (Some(split_cat_b()), b)],
                                            );
                                        match r {
                                            Ok(_) => {
                                                push_status(status, "Split.");
                                                expanded.set(None);
                                            }
                                            Err(e) => push_status(status, e.as_user_message()),
                                        }
                                    }
                                    super::bump(nonce);
                                }
                            },
                            "Split"
                        }
                    }
                }
            }
        }
    }
}

/// Dropdown that sets a category on the given transactions.
/// Debug panel: the bank's JSON for this row and its account, exactly as it was imported.
#[component]
fn RawDataPanel(store: Signal<Option<SharedStore>>, txn_id: String) -> Element {
    let raw = store().and_then(|s| {
        s.lock()
            .ok()
            .and_then(|g| g.raw_transaction(&txn_id).ok().flatten())
    });
    rsx! {
        div { class: "splits trace",
            match raw {
                None => rsx! {
                    p { class: "hint", "That transaction is gone." }
                },
                Some(r) => rsx! {
                    h3 {
                        "Transaction"
                        span { class: "hint-inline", " as the bank sent it" }
                    }
                    match &r.transaction {
                        Some(j) => rsx! {
                            pre { "{j}" }
                        },
                        None => rsx! {
                            p { class: "hint", "No raw data for this row yet. It is kept from the next sync on." }
                        },
                    }
                    h3 { "Account" }
                    match &r.account {
                        Some(j) => rsx! {
                            pre { "{j}" }
                        },
                        None => rsx! {
                            p { class: "hint", "No raw data for this account yet. It is kept from the next sync on." }
                        },
                    }
                },
            }
        }
    }
}

/// Debug panel: the recorded AI answer for this row's payee, with each request and response
/// as it went over the wire. Read straight from `ai_answers`; nothing is sent.
#[component]
fn AiTracePanel(store: Signal<Option<SharedStore>>, txn_id: String, nonce: Signal<u64>) -> Element {
    let _ = nonce();
    // A read error is shown as such, not as "never asked".
    let record = match store() {
        None => Ok(None),
        Some(s) => match s.lock() {
            Ok(g) => g.ai_record_for(&txn_id).map_err(|e| e.as_user_message()),
            Err(_) => Err("Could not read the ledger.".to_string()),
        },
    };
    rsx! {
        div { class: "splits trace",
            match record {
                Err(e) => rsx! {
                    p { class: "hint", "{e}" }
                },
                Ok(None) => rsx! {
                    p { class: "hint", "The AI has not been asked about this payee." }
                },
                Ok(Some(r)) => rsx! {
                    p { class: "hint",
                        "Asked {r.provider} about "
                        code { "{r.payee}" }
                        " {super::relative_time(r.asked_at, chrono::Utc::now().timestamp())}"
                        if r.error.is_some() {
                            ", and the call failed."
                        }
                        if r.exchanges.is_empty() && r.error.is_none() {
                            "."
                        }
                    }
                    for (i, x) in r.exchanges.iter().enumerate() {
                        if r.exchanges.len() > 1 {
                            p { class: "hint", "Attempt {i + 1}" }
                        }
                        h3 {
                            "Request"
                            span { class: "hint-inline", " {x.method} {x.url}" }
                        }
                        pre { "{x.request}" }
                        h3 {
                            "Response"
                            span { class: "hint-inline",
                                match x.status {
                                    Some(code) => format!(" HTTP {code}"),
                                    None => " no response (network error)".to_string(),
                                }
                            }
                        }
                        pre {
                            if x.response.is_empty() {
                                "(empty)"
                            } else {
                                "{x.response}"
                            }
                        }
                    }
                    if r.exchanges.is_empty() && r.error.is_none() {
                        p { class: "hint",
                            "This answer was recorded before requests were kept. Ask AI on this row to ask again and record the exchange."
                        }
                    }
                    p { class: "trace-result", "{ai_record_outcome(&r)}" }
                },
            }
        }
    }
}

/// "Picked Dining at 91% confidence.", "Picked other (nothing fits) at 40% confidence.", or
/// the error when the last call failed.
fn ai_record_outcome(r: &AiRecord) -> String {
    if let Some(e) = &r.error {
        return format!("Failed: {e} Ask AI on this row or run Setup → AI to retry.");
    }
    let pct = (r.confidence * 100.0).round() as i64;
    match (&r.category_id, &r.category_name) {
        (Some(_), Some(name)) => format!("Picked {name} at {pct}% confidence."),
        (Some(_), None) => format!("Picked a category that no longer exists at {pct}% confidence."),
        (None, _) => format!("Picked other (nothing fits) at {pct}% confidence."),
    }
}

#[component]
fn CategoryPicker(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
    ids: Vec<String>,
    cats: Vec<Category>,
    current_id: Option<String>,
    class: String,
    prompt: Option<String>,
    selection: Option<Signal<Vec<String>>>,
) -> Element {
    let bulk = prompt.is_some();
    let value = if bulk {
        String::new()
    } else {
        current_id.clone().unwrap_or_default()
    };
    rsx! {
        select {
            class: "{class}",
            aria_label: "Category",
            value: "{value}",
            onmousedown: move |e| e.stop_propagation(),
            onclick: move |e| e.stop_propagation(),
            onkeydown: move |e| e.stop_propagation(),
            onchange: move |e| {
                let v = e.value();
                if bulk && v.is_empty() {
                    return;
                }
                let cat = if v.is_empty() { None } else { Some(v) };
                apply_txn_category(store, status, nonce, &ids, cat);
                if let Some(mut selection) = selection {
                    selection.set(Vec::new());
                }
            },
            if let Some(ph) = prompt.as_ref() {
                option { value: "", disabled: true, selected: true, "{ph}" }
            } else {
                option { value: "", "-" }
            }
            for c in cats.iter() {
                option {
                    value: "{c.id}",
                    selected: !bulk && current_id.as_deref() == Some(c.id.as_str()),
                    "{c.label()}"
                }
            }
        }
    }
}

/// Set or clear a category. Learns the payee and fills similar uncategorized rows.
fn apply_txn_category(
    store: Signal<Option<SharedStore>>,
    status: Signal<StatusState>,
    nonce: Signal<u64>,
    ids: &[String],
    category_id: Option<String>,
) {
    if ids.is_empty() {
        return;
    }
    if let Some(s) = store() {
        match s.lock().unwrap().patch_transactions(
            ids,
            &TxnPatch {
                category_id: Some(category_id),
                ..Default::default()
            },
        ) {
            Ok(n) if n > 0 => push_status(status, format!("Also categorized {n} similar.")),
            Ok(_) => {}
            Err(e) => push_status(status, e.as_user_message()),
        }
    }
    super::bump(nonce);
}

/// Group rows by month so "All" is readable across a long history. Pending rows go in
/// a "Pending" group first: the bank reports them with no posted date (epoch 0), which
/// would otherwise file them under 1970 at the bottom.
fn group_rows(txns: &[Txn]) -> Vec<(String, Vec<(usize, Txn)>)> {
    let mut groups: Vec<(String, Vec<(usize, Txn)>)> = Vec::new();
    let mut pending: Vec<(usize, Txn)> = Vec::new();
    for (i, t) in txns.iter().cloned().enumerate() {
        if t.pending {
            pending.push((i, t));
            continue;
        }
        let label = chrono::DateTime::from_timestamp(t.posted_at, 0)
            .map(|d| {
                use chrono::Datelike;
                month_label(d.year(), d.month())
            })
            .unwrap_or_else(|| "Undated".into());
        match groups.last_mut() {
            Some((l, rows)) if *l == label => rows.push((i, t)),
            _ => groups.push((label, vec![(i, t)])),
        }
    }
    if !pending.is_empty() {
        groups.insert(0, ("Pending".into(), pending));
    }
    groups
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// "Sep 3" for a row date; the month header carries the year.
fn fmt_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_else(|| "-".into())
}

/// "2026-09-03" for the editor's date field.
fn fmt_iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Noon UTC on the given day, so the row stays inside that calendar day in any listing.
fn parse_iso(s: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .ok()?
        .and_hms_opt(12, 0, 0)
        .map(|d| d.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txn(id: &str) -> Txn {
        Txn {
            id: id.into(),
            account_id: "a".into(),
            account_name: "Checking".into(),
            posted_at: 0,
            amount_cents: -100,
            payee: "x".into(),
            notes: None,
            category_id: None,
            category_name: None,
            excluded: false,
            pending: false,
            is_transfer: false,
            payment: None,
            is_income: false,
            has_splits: false,
            parent_id: None,
            categorized_by: None,
            ai_failed_at: None,
        }
    }

    #[test]
    fn ai_record_outcome_names_the_pick() {
        let rec = |category_id: Option<&str>, name: Option<&str>| AiRecord {
            payee: "X".into(),
            category_id: category_id.map(String::from),
            category_name: name.map(String::from),
            confidence: 0.905,
            provider: "typesafe".into(),
            asked_at: 0,
            exchanges: vec![],
            error: None,
        };
        assert_eq!(
            ai_record_outcome(&rec(Some("c1"), Some("Dining"))),
            "Picked Dining at 91% confidence."
        );
        assert_eq!(
            ai_record_outcome(&rec(Some("c1"), None)),
            "Picked a category that no longer exists at 91% confidence."
        );
        assert_eq!(
            ai_record_outcome(&rec(None, None)),
            "Picked other (nothing fits) at 91% confidence."
        );
        let failed = AiRecord {
            error: Some("AI key was rejected.".into()),
            ..rec(None, None)
        };
        assert_eq!(
            ai_record_outcome(&failed),
            "Failed: AI key was rejected. Ask AI on this row or run Setup → AI to retry."
        );
    }

    #[test]
    fn ai_candidate_needs_a_plain_uncategorized_row() {
        assert!(ai_candidate(&txn("a")));
        let mut t = txn("b");
        t.category_id = Some("food".into());
        assert!(!ai_candidate(&t));
        let mut t = txn("c");
        t.excluded = true;
        assert!(!ai_candidate(&t));
        let mut t = txn("d");
        t.is_transfer = true;
        assert!(!ai_candidate(&t));
        let mut t = txn("e");
        t.is_income = true;
        assert!(!ai_candidate(&t));
        let mut t = txn("f");
        t.has_splits = true;
        assert!(!ai_candidate(&t));
    }

    #[test]
    fn mark_patch_resets_every_flag_and_sets_the_chosen_one() {
        assert!(mark_patch("income", "income").is_none());
        let p = mark_patch("", "card_payment").unwrap();
        assert_eq!(p.payment, Some(Some(PaymentKind::Card)));
        assert_eq!(p.transfer_account_id, Some(Some("manual".into())));
        assert_eq!(p.income, Some(false));
        assert_eq!(p.excluded, Some(false));
        let p = mark_patch("card_payment", "").unwrap();
        assert_eq!(p.transfer_account_id, Some(None));
        assert_eq!(p.payment, None);
        assert_eq!(p.excluded, Some(false));
        let p = mark_patch("transfer", "exclude").unwrap();
        assert_eq!(p.excluded, Some(true));
        assert_eq!(p.transfer_account_id, Some(None));
        assert_eq!(mark_patch("", "income").unwrap().income, Some(true));
    }

    #[test]
    fn mark_message_names_payee_and_flag() {
        assert_eq!(
            mark_message("COSTCO", "card_payment"),
            "Marked COSTCO as credit card payment."
        );
        assert_eq!(
            mark_message("COSTCO", "exclude"),
            "Marked COSTCO as excluded."
        );
        assert_eq!(mark_message("COSTCO", ""), "Unmarked COSTCO.");
    }

    #[test]
    fn current_mark_reads_the_row_flags() {
        let mut t = txn("a");
        assert_eq!(current_mark(&t), "");
        t.excluded = true;
        assert_eq!(current_mark(&t), "exclude");
        t.is_income = true;
        assert_eq!(current_mark(&t), "income");
        t.is_transfer = true;
        assert_eq!(current_mark(&t), "transfer");
        t.payment = Some(PaymentKind::Loan);
        assert_eq!(current_mark(&t), "loan_payment");
    }

    #[test]
    fn bulk_mark_patch_maps_choices() {
        assert!(bulk_mark_patch("").is_none());
        let p = bulk_mark_patch("transfer").unwrap();
        assert_eq!(p.transfer_account_id, Some(Some("manual".into())));
        assert_eq!(p.payment, Some(None));
        assert_eq!(
            bulk_mark_patch("card_payment").unwrap().payment,
            Some(Some(PaymentKind::Card))
        );
        assert_eq!(
            bulk_mark_patch("loan_payment").unwrap().payment,
            Some(Some(PaymentKind::Loan))
        );
        assert_eq!(bulk_mark_patch("income").unwrap().income, Some(true));
        assert_eq!(
            bulk_mark_patch("not_transfer").unwrap().transfer_account_id,
            Some(None)
        );
        assert_eq!(bulk_mark_patch("not_income").unwrap().income, Some(false));
    }

    #[test]
    fn rule_target_defaults_to_category_then_flag() {
        let mut t = txn("a");
        assert_eq!(default_rule_target(&t), "");
        t.is_income = true;
        assert_eq!(default_rule_target(&t), "income");
        t.is_transfer = true;
        assert_eq!(default_rule_target(&t), "transfer");
        t.payment = Some(PaymentKind::Card);
        assert_eq!(default_rule_target(&t), "card_payment");
        t.payment = Some(PaymentKind::Loan);
        assert_eq!(default_rule_target(&t), "loan_payment");
        t.category_id = Some("food".into());
        assert_eq!(default_rule_target(&t), "cat:food");
    }

    #[test]
    fn targets_prefers_selection_over_cursor() {
        let rows = vec![txn("a"), txn("b")];
        assert_eq!(targets(&[], &rows, 1), ["b"]);
        assert_eq!(targets(&["a".to_string()], &rows, 1), ["a"]);
        assert!(targets(&[], &rows, 9).is_empty());
    }

    #[test]
    fn pending_rows_group_first() {
        let day = parse_iso("2026-09-03").unwrap();
        let mut posted = txn("a");
        posted.posted_at = day;
        let mut pend = txn("b");
        pend.pending = true;
        let groups = group_rows(&[posted, pend]);
        assert_eq!(groups[0].0, "Pending");
        assert_eq!(groups[0].1[0].1.id, "b");
        assert_eq!(groups[1].0, "September 2026");
        assert_eq!(groups[1].1[0].1.id, "a");
        assert!(group_rows(&[txn("c")]).iter().all(|(l, _)| l != "Pending"));
    }

    #[test]
    fn iso_dates_round_trip() {
        let ts = parse_iso("2026-09-03").unwrap();
        assert_eq!(fmt_iso(ts), "2026-09-03");
        assert_eq!(fmt_day(ts), "Sep 3");
        assert!(parse_iso("nope").is_none());
    }
}
