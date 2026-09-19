use dioxus::prelude::*;

use crate::ui::status::{push_status, StatusState};
use crate::SharedStore;
use myphin::domain::{Category, Txn};
use myphin::money::format_cents;
use myphin::{Rule, RuleAction};

/// Rules tab: the rule form with live preview, and the list of existing rules.
#[component]
pub fn Rules(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
) -> Element {
    // One entry per alternative the rule checks for; blanks are dropped on save.
    let mut rule_patterns: Signal<Vec<String>> = use_signal(|| vec![String::new()]);
    // "cat:<id>" for a categorize rule, else the action name.
    let mut rule_target = use_signal(String::new);
    // Id of the rule loaded into the form, if the form is editing rather than adding.
    let mut editing: Signal<Option<String>> = use_signal(|| None);
    // Index of the pattern input that has focus, so the matches column can narrow to it.
    let focused: Signal<Option<usize>> = use_signal(|| None);
    let _ = nonce();

    // Rows the form would apply to, refreshed on every keystroke: the focused alternative alone
    // when one input has focus, else every alternative together.
    let pattern = join_patterns(&rule_patterns.read());
    let focused_alt = focused_pattern(focused(), &rule_patterns.read());
    let shown_pattern = focused_alt.clone().unwrap_or_else(|| pattern.clone());
    let preview = store()
        .and_then(|s| {
            s.lock()
                .ok()
                .and_then(|g| g.preview_rule(&shown_pattern).ok())
        })
        .unwrap_or_default();
    let suggestion = store()
        .and_then(|s| {
            s.lock()
                .ok()
                .and_then(|g| g.suggest_contains_pattern(&shown_pattern).ok())
        })
        .flatten();

    let (cats, rules) = store()
        .and_then(|s| {
            s.lock().ok().map(|g| {
                (
                    g.list_categories().unwrap_or_default(),
                    g.list_rules().unwrap_or_default(),
                )
            })
        })
        .unwrap_or_default();
    let rule_count = rules.len();

    let mut reset_form = move || {
        rule_patterns.set(vec![String::new()]);
        rule_target.set(String::new());
        editing.set(None);
    };

    // Add a new rule, or save the one being edited, from what is in the form.
    let mut submit = move |_| {
        let pattern = join_patterns(&rule_patterns.read());
        let (action, category_id) = parse_rule_target(&rule_target.read());
        if pattern.is_empty() || (action == RuleAction::Category && category_id.is_none()) {
            push_status(status, "Need a pattern and an action.");
            return;
        }
        if let Some(s) = store() {
            let st = s.lock().unwrap();
            let msg = match editing() {
                Some(id) => rule_saved_message(st.update_rule(&id, &pattern, action, category_id)),
                None => rule_added_message(st.create_rule(&Rule::pattern(
                    &pattern,
                    action,
                    category_id,
                    rule_count as i64 + 1,
                ))),
            };
            push_status(status, msg);
        }
        reset_form();
        super::bump(nonce);
    };

    rsx! {
        section {
            h2 { "Rules" }
            p { class: "hint",
                "When a bank description matches the pattern, the row gets the category, or is marked as a transfer, credit card payment, loan payment, income, or excluded. "
                code { "*" }
                " stands for anything and "
                code { "?" }
                " for one character, so "
                code { "*costco*" }
                " matches any description with costco in it. Add more patterns to give them all the same action; a description only has to match one of them. Case doesn't matter. A new rule is applied to every transaction it matches right away, including ones you categorized by hand. Rules win over learned payees; earlier rules win over later ones. Deleting a rule leaves rows as they are."
            }
            div { class: "rule-form-layout",
                div { class: "rule-form-col",
            p { class: "hint", "If description matches" }
            PatternList {
                patterns: rule_patterns,
                focused,
                on_submit: move |_| submit(()),
                on_cancel: move |_| if editing().is_some() { reset_form() },
            }
            div { class: "row rule-form",
                span { class: "hint", "then" }
                select {
                    aria_label: "Rule action",
                    value: "{rule_target}",
                    onchange: move |e| rule_target.set(e.value()),
                    option { value: "", "pick an action" }
                    optgroup { label: "Mark as",
                        option { value: "transfer", selected: rule_target() == "transfer", "transfer" }
                        option { value: "card_payment", selected: rule_target() == "card_payment", "credit card payment" }
                        option { value: "loan_payment", selected: rule_target() == "loan_payment", "loan payment" }
                        option { value: "income", selected: rule_target() == "income", "income / paycheck" }
                        option { value: "exclude", selected: rule_target() == "exclude", "excluded" }
                    }
                    if !cats.is_empty() {
                        optgroup { label: "Categorize as",
                            for c in cats.iter() {
                                option { value: "cat:{c.id}", selected: rule_target() == format!("cat:{}", c.id), "{c.label()}" }
                            }
                        }
                    }
                }
                if editing().is_some() {
                    button { class: "primary", onclick: move |_| submit(()), "Save rule" }
                    button { class: "ghost", onclick: move |_| reset_form(), "Cancel" }
                } else {
                    button { class: "primary", onclick: move |_| submit(()), "Add rule" }
                }
            }
                }
                div { class: "rule-matches", aria_label: "Matching transactions",
                    h3 { "{matches_heading(editing().is_some(), &pattern, focused_alt.as_deref())}" }
                    if shown_pattern.is_empty() {
                        p { class: "hint", "Pick Edit on a rule, or type a pattern, to see every transaction it matches. Click into one pattern to see only its matches." }
                    } else {
                        RulePreview { rows: preview, suggestion, shown: usize::MAX }
                    }
                }
            }
            if rules.is_empty() {
                p { class: "empty", "No rules yet. Categorizing in Activity teaches payees on its own; rules are for the stubborn ones, transfers, card and loan payments, and paychecks." }
            } else {
                ol { class: "rules",
                    for r in rules {
                        {
                            let outcome = rule_outcome(&r, &cats);
                            let full_pattern = r.description_pattern.clone().unwrap_or_default();
                            let short_pattern = truncate_pattern(&full_pattern, PATTERN_CHARS);
                            let rid = r.id.clone();
                            let edit_id = r.id.clone();
                            let edit_patterns = split_rule_patterns(r.description_pattern.as_deref().unwrap_or_default());
                            let edit_target = rule_target_for(&r);
                            let is_editing = editing().as_deref() == Some(r.id.as_str());
                            rsx! {
                                li { key: "{rid}", class: if is_editing { "editing" } else { "" },
                                    span { class: "rule-text", title: "{full_pattern}",
                                        strong { "{outcome}" }
                                        span { class: "hint", ": " }
                                        code { "{short_pattern}" }
                                    }
                                    button {
                                        class: "ghost small",
                                        aria_label: "Edit rule",
                                        onclick: move |_| {
                                            rule_patterns.set(edit_patterns.clone());
                                            rule_target.set(edit_target.clone());
                                            editing.set(Some(edit_id.clone()));
                                        },
                                        "Edit"
                                    }
                                    button {
                                        class: "danger small",
                                        aria_label: "Delete rule",
                                        onclick: move |_| {
                                            if let Some(s) = store() {
                                                match s.lock().unwrap().delete_rule(&rid) {
                                                    Ok(_) => {
                                                        if editing().as_deref() == Some(rid.as_str()) {
                                                            reset_form();
                                                        }
                                                        super::bump(nonce)
                                                    }
                                                    Err(e) => push_status(status, e.as_user_message()),
                                                }
                                            }
                                        },
                                        "Delete"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The alternative in the focused pattern input, canonical and on its own, or `None` when no
/// input has focus or the focused one is blank (the column then shows the whole rule).
pub(super) fn focused_pattern(focused: Option<usize>, patterns: &[String]) -> Option<String> {
    let row = patterns.get(focused?)?;
    let alt = join_patterns(std::slice::from_ref(row));
    (!alt.is_empty()).then_some(alt)
}

/// Title of the matches column: the focused alternative when there is one, else whether the
/// list is for a saved rule being edited or a pattern still being typed.
pub(super) fn matches_heading(editing: bool, pattern: &str, focused_alt: Option<&str>) -> String {
    match focused_alt {
        Some(alt) => format!("{alt} matches"),
        None if pattern.is_empty() => "Matching transactions".into(),
        None if editing => "This rule matches".into(),
        None => "This pattern matches".into(),
    }
}

/// The alternatives of a rule pattern, one input each, so a long rule can be edited piece by
/// piece: a Remove button on every row, an Add button that appends a blank row and focuses it,
/// and "or" in front of every row but the first. Enter in any input fires `on_submit`, Escape
/// fires `on_cancel`. The list never goes empty; removing the last row leaves one blank. When
/// `focused` is given, it holds the index of the input that has focus.
#[component]
pub(super) fn PatternList(
    patterns: Signal<Vec<String>>,
    focused: Option<Signal<Option<usize>>>,
    on_submit: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    // Index of the row Add just made, so it takes focus once it is in the DOM.
    let mut focus_at: Signal<Option<usize>> = use_signal(|| None);
    let count = patterns.read().len();
    rsx! {
        ul { class: "pattern-list", aria_label: "Patterns",
            for i in 0..count {
                li { key: "{i}",
                    span { class: "or", aria_hidden: "true", if i > 0 { "or" } }
                    input {
                        value: "{patterns.read()[i]}",
                        placeholder: "*costco*",
                        aria_label: if i == 0 { "Pattern".to_string() } else { format!("Pattern {}", i + 1) },
                        spellcheck: "false",
                        autocorrect: "off",
                        autocapitalize: "off",
                        autocomplete: "off",
                        oninput: move |e| patterns.write()[i] = e.value(),
                        onfocus: move |_| if let Some(mut f) = focused { f.set(Some(i)) },
                        onblur: move |_| if let Some(mut f) = focused { f.set(None) },
                        onkeydown: move |e| match e.key() {
                            Key::Enter => on_submit.call(()),
                            Key::Escape => on_cancel.call(()),
                            _ => {}
                        },
                        onmounted: move |e| {
                            if *focus_at.peek() != Some(i) {
                                return;
                            }
                            focus_at.set(None);
                            spawn(async move {
                                let _ = e.data().set_focus(true).await;
                            });
                        },
                    }
                    button {
                        class: "ghost small",
                        aria_label: "Remove pattern",
                        onclick: move |_| {
                            let mut list = patterns.write();
                            list.remove(i);
                            if list.is_empty() {
                                list.push(String::new());
                            }
                        },
                        "Remove"
                    }
                }
            }
        }
        button {
            class: "ghost small pattern-add",
            onclick: move |_| {
                focus_at.set(Some(count));
                patterns.write().push(String::new());
            },
            "+ Add another pattern"
        }
    }
}

/// The alternatives in a rule form joined into the canonical `a | b` pattern the store takes.
/// Blank rows are skipped, so a form with only blanks gives an empty pattern.
pub(super) fn join_patterns(patterns: &[String]) -> String {
    patterns
        .iter()
        .flat_map(|p| myphin::domain::split_patterns(p))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A stored pattern as rows for the form, one per alternative; an empty pattern gives one blank
/// row so there is always something to type into. Inverse of [`join_patterns`].
pub(super) fn split_rule_patterns(pattern: &str) -> Vec<String> {
    let rows: Vec<String> = myphin::domain::split_patterns(pattern)
        .map(str::to_string)
        .collect();
    if rows.is_empty() {
        vec![String::new()]
    } else {
        rows
    }
}

/// The rule action picked in a rule form: "cat:<id>" for a categorize rule, else the action name.
pub(super) fn parse_rule_target(target: &str) -> (RuleAction, Option<String>) {
    match target {
        "transfer" => (RuleAction::Transfer, None),
        "card_payment" => (RuleAction::CardPayment, None),
        "loan_payment" => (RuleAction::LoanPayment, None),
        "income" => (RuleAction::Income, None),
        "exclude" => (RuleAction::Exclude, None),
        t if t.starts_with("cat:") => (RuleAction::Category, Some(t["cat:".len()..].to_string())),
        _ => (RuleAction::Category, None),
    }
}

/// The form value for an existing rule's action: "cat:<id>" for a categorize rule, else the
/// action name. Inverse of [`parse_rule_target`].
pub(super) fn rule_target_for(rule: &Rule) -> String {
    match rule.action {
        RuleAction::Category => format!("cat:{}", rule.category_id.clone().unwrap_or_default()),
        other => other.as_str().to_string(),
    }
}

/// Toast text for a [`Store::update_rule`] result: what happened, and how many rows changed.
pub(super) fn rule_saved_message(result: Result<u32, myphin::Error>) -> String {
    match result {
        Ok(0) => "Rule saved. No rows changed.".into(),
        Ok(1) => "Rule saved. Applied to 1 row.".into(),
        Ok(n) => format!("Rule saved. Applied to {n} rows."),
        Err(e) => e.as_user_message(),
    }
}

/// Toast text for a [`Store::add_rule_pattern`] result: what happened, and how many rows changed.
pub(super) fn rule_extended_message(result: Result<u32, myphin::Error>) -> String {
    match result {
        Ok(0) => "Added to rule. No rows changed.".into(),
        Ok(1) => "Added to rule. Applied to 1 row.".into(),
        Ok(n) => format!("Added to rule. Applied to {n} rows."),
        Err(e) => e.as_user_message(),
    }
}

/// What a rule does, for lists and pickers: the category name, or the flag it sets.
pub(super) fn rule_outcome(rule: &Rule, cats: &[Category]) -> String {
    match rule.action {
        RuleAction::Category => cats
            .iter()
            .find(|c| Some(&c.id) == rule.category_id.as_ref())
            .map(|c| c.label())
            .unwrap_or_else(|| "(deleted)".into()),
        other => other.label().to_string(),
    }
}

/// One-line label for a rule in lists and pickers: outcome first, then the pattern cut short,
/// like `Gas: *ARCO* | *COSTCO GA…`.
pub(super) fn rule_summary(rule: &Rule, cats: &[Category]) -> String {
    let pattern = rule.description_pattern.as_deref().unwrap_or_default();
    format!(
        "{}: {}",
        rule_outcome(rule, cats),
        truncate_pattern(pattern, PATTERN_CHARS)
    )
}

/// How much of a rule's pattern the list shows before cutting it off.
const PATTERN_CHARS: usize = 40;

/// The canonical `a | b` pattern cut to `max` characters with an ellipsis, so long rules stay on one line.
pub(super) fn truncate_pattern(pattern: &str, max: usize) -> String {
    if pattern.chars().count() <= max {
        return pattern.to_string();
    }
    let cut: String = pattern.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Toast text for a [`Store::create_rule`] result: what happened, and how many rows changed.
pub(super) fn rule_added_message(result: Result<(String, u32), myphin::Error>) -> String {
    match result {
        Ok((_, 0)) => "Rule added. No rows changed.".into(),
        Ok((_, 1)) => "Rule added. Applied to 1 row.".into(),
        Ok((_, n)) => format!("Rule added. Applied to {n} rows."),
        Err(e) => e.as_user_message(),
    }
}

/// Rows the pattern being typed would apply to, newest first. `shown` caps the list so a form
/// stays short; the Rules matches column passes `usize::MAX` to list every row.
#[component]
pub(super) fn RulePreview(
    rows: Vec<Txn>,
    suggestion: Option<(String, usize)>,
    #[props(default = 8)] shown: usize,
) -> Element {
    let total = rows.len();
    rsx! {
        div { class: "rule-preview", aria_live: "polite",
            if total == 0 {
                p { class: "hint",
                    "No transactions match this pattern. Patterns match the whole description"
                    if let Some((wrapped, n)) = suggestion {
                        ", so "
                        code { "{wrapped}" }
                        if n == 1 { " would match 1 transaction." } else { " would match {n} transactions." }
                    } else {
                        ", so wrap it in "
                        code { "*" }
                        " to match part of one."
                    }
                }
            } else {
                p { class: "hint",
                    if total == 1 { "Matches 1 transaction:" } else { "Matches {total} transactions:" }
                }
                ul {
                    for t in rows.iter().take(shown) {
                        li { key: "{t.id}",
                            span { class: "date", "{fmt_day(t.posted_at)}" }
                            span { class: "payee", "{t.payee}" }
                            span { class: "amount", "${format_cents(t.amount_cents.abs())}" }
                        }
                    }
                }
                if total > shown {
                    p { class: "hint", "and {total - shown} more" }
                }
            }
        }
    }
}

/// "Sep 3, 2026" for a preview row.
fn fmt_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .filter(|_| ts > 0)
        .map(|d| d.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|| "Pending".into())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_target_parses_flags_and_categories() {
        assert_eq!(parse_rule_target("transfer"), (RuleAction::Transfer, None));
        assert_eq!(
            parse_rule_target("card_payment"),
            (RuleAction::CardPayment, None)
        );
        assert_eq!(
            parse_rule_target("loan_payment"),
            (RuleAction::LoanPayment, None)
        );
        assert_eq!(parse_rule_target("income"), (RuleAction::Income, None));
        assert_eq!(parse_rule_target("exclude"), (RuleAction::Exclude, None));
        assert_eq!(
            parse_rule_target("cat:abc"),
            (RuleAction::Category, Some("abc".into()))
        );
        assert_eq!(parse_rule_target(""), (RuleAction::Category, None));
    }

    #[test]
    fn rule_target_for_round_trips() {
        let cat = Rule::pattern("x", RuleAction::Category, Some("abc".into()), 1);
        assert_eq!(rule_target_for(&cat), "cat:abc");
        assert_eq!(
            parse_rule_target(&rule_target_for(&cat)),
            (RuleAction::Category, Some("abc".into()))
        );
        for action in [
            RuleAction::Transfer,
            RuleAction::CardPayment,
            RuleAction::LoanPayment,
            RuleAction::Income,
            RuleAction::Exclude,
        ] {
            let r = Rule::pattern("x", action, None, 1);
            assert_eq!(parse_rule_target(&rule_target_for(&r)), (action, None));
        }
    }

    #[test]
    fn rule_saved_message_counts_rows() {
        assert_eq!(rule_saved_message(Ok(0)), "Rule saved. No rows changed.");
        assert_eq!(rule_saved_message(Ok(1)), "Rule saved. Applied to 1 row.");
        assert_eq!(rule_saved_message(Ok(3)), "Rule saved. Applied to 3 rows.");
        assert_eq!(rule_saved_message(Err(myphin::Error::user("nope"))), "nope");
    }

    #[test]
    fn rule_extended_message_counts_rows() {
        assert_eq!(
            rule_extended_message(Ok(0)),
            "Added to rule. No rows changed."
        );
        assert_eq!(
            rule_extended_message(Ok(1)),
            "Added to rule. Applied to 1 row."
        );
        assert_eq!(
            rule_extended_message(Ok(3)),
            "Added to rule. Applied to 3 rows."
        );
    }

    #[test]
    fn rule_summary_puts_outcome_first_and_truncates() {
        let cats = vec![Category {
            id: "food".into(),
            name: "Food".into(),
            description: None,
            parent_id: None,
            parent_name: None,
            in_budget: true,
            parent_in_budget: true,
        }];
        let r = Rule::pattern(
            "*costco* | arco*",
            RuleAction::Category,
            Some("food".into()),
            1,
        );
        assert_eq!(rule_summary(&r, &cats), "Food: *costco* | arco*");
        let r = Rule::pattern("payroll*", RuleAction::Income, None, 2);
        assert_eq!(rule_summary(&r, &cats), "mark income: payroll*");
        let r = Rule::pattern("x", RuleAction::Category, Some("gone".into()), 3);
        assert_eq!(rule_summary(&r, &cats), "(deleted): x");
        let long = "*ARCO* | *COSTCO GAS* | *SHELL OIL* | *CHEVRON* | *76 STATION*";
        let r = Rule::pattern(long, RuleAction::Category, Some("food".into()), 4);
        assert_eq!(
            rule_summary(&r, &cats),
            "Food: *ARCO* | *COSTCO GAS* | *SHELL OIL* | *C…"
        );
    }

    #[test]
    fn rule_outcome_names_category_or_flag() {
        let cats = vec![Category {
            id: "gas".into(),
            name: "Gas".into(),
            description: None,
            parent_id: None,
            parent_name: None,
            in_budget: true,
            parent_in_budget: true,
        }];
        let r = Rule::pattern("*arco*", RuleAction::Category, Some("gas".into()), 1);
        assert_eq!(rule_outcome(&r, &cats), "Gas");
        // A sub-category shows with its parent so two "Fees" never look alike.
        let mut cats = cats;
        cats.push(Category {
            id: "diesel".into(),
            name: "Diesel".into(),
            description: None,
            parent_id: Some("gas".into()),
            parent_name: Some("Gas".into()),
            in_budget: true,
            parent_in_budget: true,
        });
        let r = Rule::pattern("*truck*", RuleAction::Category, Some("diesel".into()), 4);
        assert_eq!(rule_outcome(&r, &cats), "Gas › Diesel");
        let r = Rule::pattern("*", RuleAction::Exclude, None, 2);
        assert_eq!(rule_outcome(&r, &cats), "exclude");
        let r = Rule::pattern("x", RuleAction::Category, Some("gone".into()), 3);
        assert_eq!(rule_outcome(&r, &cats), "(deleted)");
    }

    #[test]
    fn truncate_pattern_cuts_long_alternatives() {
        assert_eq!(
            truncate_pattern("*arco* | *costco*", 40),
            "*arco* | *costco*"
        );
        assert_eq!(
            truncate_pattern("*ARCO* | *COSTCO GAS* | *SHELL OIL*", 19),
            "*ARCO* | *COSTCO GA…"
        );
        // A cut that lands on a space does not leave a dangling gap before the ellipsis.
        assert_eq!(truncate_pattern("abcdef | ghi", 7), "abcdef…");
        assert_eq!(truncate_pattern("", 5), "");
    }

    #[test]
    fn join_and_split_patterns_round_trip() {
        assert_eq!(
            join_patterns(&["*costco*".into(), " arco shop* ".into()]),
            "*costco* | arco shop*"
        );
        // Blank rows are dropped, and a `|` typed inside one row still splits.
        assert_eq!(
            join_patterns(&["".into(), "a|b".into(), "  ".into()]),
            "a | b"
        );
        assert_eq!(join_patterns(&[String::new()]), "");
        assert_eq!(
            split_rule_patterns("*costco* | arco shop*"),
            vec!["*costco*", "arco shop*"]
        );
        assert_eq!(split_rule_patterns(""), vec![""]);
        assert_eq!(split_rule_patterns(" | "), vec![""]);
        let rows = split_rule_patterns("a | b | c");
        assert_eq!(join_patterns(&rows), "a | b | c");
    }

    #[test]
    fn matches_heading_names_focused_alternative_rule_or_pattern() {
        assert_eq!(matches_heading(false, "", None), "Matching transactions");
        assert_eq!(matches_heading(true, "", None), "Matching transactions");
        assert_eq!(matches_heading(true, "*costco*", None), "This rule matches");
        assert_eq!(
            matches_heading(false, "*costco*", None),
            "This pattern matches"
        );
        assert_eq!(
            matches_heading(true, "*costco* | *arco*", Some("*arco*")),
            "*arco* matches"
        );
    }

    #[test]
    fn focused_pattern_is_the_focused_row_alone() {
        let rows = vec!["*costco*".into(), " *ARCO* ".into(), "".into()];
        assert_eq!(focused_pattern(None, &rows), None);
        assert_eq!(focused_pattern(Some(1), &rows), Some("*ARCO*".into()));
        // A blank row and an index past the end fall back to the whole rule.
        assert_eq!(focused_pattern(Some(2), &rows), None);
        assert_eq!(focused_pattern(Some(9), &rows), None);
    }

    #[test]
    fn rule_added_message_counts_rows() {
        assert_eq!(
            rule_added_message(Ok(("id".into(), 0))),
            "Rule added. No rows changed."
        );
        assert_eq!(
            rule_added_message(Ok(("id".into(), 1))),
            "Rule added. Applied to 1 row."
        );
        assert_eq!(
            rule_added_message(Ok(("id".into(), 7))),
            "Rule added. Applied to 7 rows."
        );
        assert_eq!(rule_added_message(Err(myphin::Error::user("nope"))), "nope");
    }
}
