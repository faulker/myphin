use dioxus::prelude::*;

use crate::ui::status::StatusState;
use crate::ui::{month_label, step_month, Screen};
use crate::SharedStore;
use myphin::domain::current_year_month;
use myphin::money::format_cents;
use myphin::TxnQuery;

#[component]
pub fn MonthView(
    store: Signal<Option<SharedStore>>,
    screen: Signal<Screen>,
    filter: Signal<TxnQuery>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
    activity_scroll: Signal<f64>,
) -> Element {
    let mut screen = screen;
    let mut filter = filter;
    let mut activity_scroll = activity_scroll;
    let mut ym = use_signal(current_year_month);
    let _ = nonce();
    let _ = status;
    let (year, month) = ym();
    let is_now = ym() == current_year_month();

    let (rows, todo, income) = match store() {
        Some(s) => {
            let g = s.lock().unwrap();
            // Off-budget categories (and everything under one) stay off this screen.
            let mut rows = g.month_budget(year, month).unwrap_or_default();
            rows.retain(|r| r.in_budget);
            let income = g.month_income(year, month).unwrap_or(0);
            let todo = g
                .query_transactions(&TxnQuery {
                    uncategorized_only: true,
                    month: Some((year, month)),
                    ..Default::default()
                })
                .map(|v| v.len())
                .unwrap_or(0);
            (rows, todo, income)
        }
        None => return rsx! { p { "Locked" } },
    };

    let capped: Vec<_> = rows.iter().filter(|r| r.cap_cents > 0).collect();
    // A parent row already holds its children's spend, so only top-level rows are summed.
    let spent_total: i64 = rows
        .iter()
        .filter(|r| r.parent_id.is_none())
        .map(|r| r.spent_cents)
        .sum();
    let cap_total: i64 = capped.iter().map(|r| r.cap_cents).sum();
    let over_count = capped
        .iter()
        .filter(|r| r.spent_cents > r.cap_cents)
        .count();

    let mut go = move |forward: bool| {
        let (y, m) = ym();
        ym.set(step_month(y, m, forward));
    };
    let mut open_activity = move |category_id: Option<String>, uncategorized_only: bool| {
        filter.set(TxnQuery {
            uncategorized_only,
            category_id,
            month: Some(ym()),
            ..Default::default()
        });
        // A fresh filtered list, so it starts from the top rather than a remembered offset.
        activity_scroll.set(0.0);
        screen.set(Screen::Activity);
    };

    rsx! {
        div {
            class: "month",
            tabindex: "0",
            autofocus: true,
            onkeydown: move |evt| match evt.key() {
                Key::ArrowLeft => go(false),
                Key::ArrowRight => go(true),
                _ => {}
            },
            div { class: "month-head",
                button { class: "ghost icon", aria_label: "Previous month", onclick: move |_| go(false), "‹" }
                h2 { "{month_label(year, month)}" }
                button { class: "ghost icon", aria_label: "Next month", onclick: move |_| go(true), "›" }
                if !is_now {
                    button { class: "chip", onclick: move |_| ym.set(current_year_month()), "This month" }
                }
                span { class: "hint-inline spacer", "← → change month" }
            }

            div { class: "month-summary",
                div { class: "stat",
                    span { class: "k", "Spent" }
                    span { class: "v", "${format_cents(spent_total)}" }
                }
                if income > 0 {
                    div { class: "stat",
                        span { class: "k", "Income" }
                        span { class: "v", "${format_cents(income)}" }
                    }
                }
                if cap_total > 0 {
                    div { class: "stat",
                        span { class: "k", "Capped" }
                        span { class: "v", "${format_cents(cap_total)}" }
                    }
                    div { class: if over_count > 0 { "stat warn" } else { "stat" },
                        span { class: "k", if over_count > 0 { "Over cap" } else { "Left" } }
                        span { class: "v",
                            if over_count > 0 {
                                "{over_count} {plural(over_count, \"category\", \"categories\")}"
                            } else {
                                "${format_cents((cap_total - spent_total).max(0))}"
                            }
                        }
                    }
                }
                if todo > 0 {
                    button {
                        class: "stat action",
                        onclick: move |_| open_activity(None, true),
                        span { class: "k", "Uncategorized" }
                        span { class: "v", "{todo} to review →" }
                    }
                }
            }

            if rows.is_empty() {
                div { class: "empty-state",
                    p { "No categories yet." }
                    button { class: "primary", onclick: move |_| screen.set(Screen::Categories), "Add categories" }
                }
            } else if capped.is_empty() {
                p { class: "hint", "No caps set for this month. Set them in Categories & Rules." }
            }

            ul { class: "caps",
                for row in rows {
                    {
                        let has_cap = row.cap_cents > 0;
                        let over = has_cap && row.spent_cents > row.cap_cents;
                        let left = row.cap_cents - row.spent_cents;
                        let cid = row.category_id.clone();
                        let mut class = String::from(if over { "over" } else if !has_cap { "uncapped" } else { "" });
                        if row.parent_id.is_some() {
                            class.push_str(" child");
                        }
                        rsx! {
                            li { class: "{class}",
                                button {
                                    class: "cap-row",
                                    onclick: move |_| open_activity(Some(cid.clone()), false),
                                    span { class: "cap-name", "{row.category_name}" }
                                    span { class: "cap-nums",
                                        span { class: "spent", "${format_cents(row.spent_cents)}" }
                                        if has_cap {
                                            span { class: "sep", " / " }
                                            span { class: "cap", "${format_cents(row.cap_cents)}" }
                                        }
                                    }
                                    span { class: "cap-left",
                                        if !has_cap {
                                            "no cap"
                                        } else if over {
                                            "${format_cents(-left)} over"
                                        } else {
                                            "${format_cents(left)} left"
                                        }
                                    }
                                    span { class: "bar", role: "presentation",
                                        span { class: "fill", style: "width: {pct(row.spent_cents, row.cap_cents)}%" }
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

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

fn pct(spent: i64, cap: i64) -> i64 {
    if cap <= 0 {
        return 0;
    }
    ((spent * 100) / cap).clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_clamps() {
        assert_eq!(pct(50, 100), 50);
        assert_eq!(pct(500, 100), 100);
        assert_eq!(pct(-5, 100), 0);
        assert_eq!(pct(10, 0), 0);
    }
}
