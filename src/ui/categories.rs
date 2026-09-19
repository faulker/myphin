use std::collections::HashMap;

use dioxus::prelude::*;

use crate::ui::status::{push_status, StatusState};
use crate::ui::{month_label, step_month};
use crate::SharedStore;
use myphin::domain::{Category, MonthRow};
use myphin::money::{format_cents, parse_cents};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Categories,
    Rules,
}

/// Categories & Rules screen: two sub-tabs, one for names and monthly caps, one for
/// description rules.
#[component]
pub fn CategoriesView(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
) -> Element {
    let mut tab = use_signal(|| Tab::Categories);
    let _ = nonce();
    if store.read().is_none() {
        return rsx! {
            p { "Locked" }
        };
    }

    rsx! {
        div { class: "setup",
            nav { class: "subnav", aria_label: "Categories & Rules sections",
                for (t, label) in [(Tab::Categories, "Categories & caps"), (Tab::Rules, "Rules")] {
                    button {
                        class: if tab() == t { "sub on" } else { "sub" },
                        onclick: move |_| tab.set(t),
                        "{label}"
                    }
                }
            }
            match tab() {
                Tab::Categories => rsx! {
                    Categories { store, nonce, status }
                },
                Tab::Rules => rsx! {
                    crate::ui::rules::Rules { store, nonce, status }
                },
            }
        }
    }
}

/// Value of the rename parent `<select>` and of `new_parent`: blank means top level,
/// otherwise a category id.
const TOP_LEVEL: &str = "";

/// The top-level category whose group ends at row `i`: the row itself when it is top level
/// and no sub-category follows, or its parent when it is the parent's last sub-category.
/// `None` while more of the group follows. Rows come parent-first, as `month_budget` orders
/// them, so the sub-category add row can be placed right after the group.
fn group_closed_at(rows: &[MonthRow], i: usize) -> Option<&str> {
    let r = rows.get(i)?;
    let group = r.parent_id.as_deref().unwrap_or(&r.category_id);
    let more = rows
        .get(i + 1)
        .is_some_and(|n| n.parent_id.as_deref() == Some(group));
    (!more).then_some(group)
}

/// Focus a just-mounted input. HTML `autofocus` is ignored when the row appears after a
/// click, so Add would otherwise leave the caret off the name.
fn focus_mounted(e: Event<MountedData>) {
    spawn(async move {
        let _ = e.data().set_focus(true).await;
    });
}

#[component]
fn Categories(
    store: Signal<Option<SharedStore>>,
    nonce: Signal<u64>,
    status: Signal<StatusState>,
) -> Element {
    // An add row is open: a name input sits below the last category (top level) or below the
    // parent's last sub-category (`new_parent` holds its id) until Enter or Esc.
    let mut adding = use_signal(|| false);
    let mut cat_name = use_signal(String::new);
    let mut new_parent = use_signal(|| TOP_LEVEL.to_string());
    let mut editing = use_signal(|| None::<String>);
    let mut edit_name = use_signal(String::new);
    let mut confirm_delete = use_signal(|| None::<String>);
    let mut ym = use_signal(myphin::current_year_month);
    let _ = nonce();
    let (year, month) = ym();
    let prev = step_month(year, month, false);

    let (rows, cats): (Vec<MonthRow>, HashMap<String, Category>) = store()
        .and_then(|s| {
            s.lock().ok().and_then(|g| {
                let rows = g.month_budget(year, month).ok()?;
                let cats = g
                    .list_categories()
                    .ok()?
                    .into_iter()
                    .map(|c| (c.id.clone(), c))
                    .collect();
                Some((rows, cats))
            })
        })
        .unwrap_or_default();
    let capped = rows.iter().filter(|r| r.cap_cents > 0).count();
    let top_level: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.parent_id.is_none())
        .map(|r| (r.category_id.clone(), r.category_name.clone()))
        .collect();

    let mut add = move |_| {
        let name = cat_name.read().trim().to_string();
        if name.is_empty() {
            push_status(status, "Category name can't be empty.");
            return;
        }
        if let Some(s) = store() {
            let parent = new_parent();
            let g = s.lock().unwrap();
            let result = if parent == TOP_LEVEL {
                g.add_category(&name)
            } else {
                g.add_subcategory(&parent, &name)
            };
            match result {
                Ok(_) => {
                    cat_name.set(String::new());
                    adding.set(false);
                    push_status(status, format!("Added {name}."));
                    super::bump(nonce);
                }
                Err(e) => push_status(status, e.as_user_message()),
            }
        }
    };
    // Open the add row at the top level, or under `parent` when the "+" on a category was
    // clicked.
    let mut open_add = move |parent: String| {
        cat_name.set(String::new());
        new_parent.set(parent);
        editing.set(None);
        confirm_delete.set(None);
        adding.set(true);
    };
    // The name input and its Save/Cancel, shared by the top-level add row and the
    // sub-category one.
    let add_input = move |placeholder: &'static str| {
        rsx! {
            td { class: "order", "" }
            td { class: "name",
                div { class: "name-edit",
                    input {
                        value: "{cat_name}",
                        placeholder: "{placeholder}",
                        aria_label: "New category name",
                        autofocus: true,
                        spellcheck: "false",
                        oninput: move |e| cat_name.set(e.value()),
                        onkeydown: move |e| match e.key() {
                            Key::Enter => add(()),
                            Key::Escape => adding.set(false),
                            _ => {}
                        },
                        onmounted: focus_mounted,
                    }
                }
            }
            td { colspan: "4",
                span { class: "hint", "Enter saves, Esc cancels." }
            }
            td { class: "actions",
                button {
                    class: "primary small",
                    onclick: move |_| add(()),
                    "Save"
                }
                button {
                    class: "ghost small",
                    onclick: move |_| adding.set(false),
                    "Cancel"
                }
            }
        }
    };
    // Shift a category one step among its siblings and re-render. Nothing to say when it is
    // already at that end.
    let move_cat = move |id: String, delta: i32| {
        if let Some(s) = store() {
            if let Err(e) = s.lock().unwrap().move_category(&id, delta) {
                push_status(status, e.as_user_message());
            }
            super::bump(nonce);
        }
    };
    let mut save_rename = move |id: String| {
        let name = edit_name.read().clone();
        if let Some(s) = store() {
            match s.lock().unwrap().rename_category(&id, &name) {
                Ok(_) => {
                    editing.set(None);
                    super::bump(nonce);
                }
                Err(e) => push_status(status, e.as_user_message()),
            }
        }
    };
    // Re-parent applies as soon as the select changes; the rename input stays open.
    let set_parent = move |id: String, parent: String| {
        if let Some(s) = store() {
            let parent = if parent == TOP_LEVEL {
                None
            } else {
                Some(parent.as_str())
            };
            if let Err(e) = s.lock().unwrap().set_category_parent(&id, parent) {
                push_status(status, e.as_user_message());
            }
            super::bump(nonce);
        }
    };
    let set_in_budget = move |id: String, on: bool| {
        if let Some(s) = store() {
            if let Err(e) = s.lock().unwrap().set_category_in_budget(&id, on) {
                push_status(status, e.as_user_message());
            }
            super::bump(nonce);
        }
    };

    rsx! {
        section {
            h2 { "Categories" }

            div { class: "caps-head",
                span { "Caps for" }
                button {
                    class: "ghost icon",
                    aria_label: "Previous month",
                    onclick: move |_| ym.set(prev),
                    "‹"
                }
                strong { "{month_label(year, month)}" }
                button {
                    class: "ghost icon",
                    aria_label: "Next month",
                    onclick: move |_| ym.set(step_month(year, month, true)),
                    "›"
                }
                if ym() != myphin::current_year_month() {
                    button {
                        class: "chip",
                        onclick: move |_| ym.set(myphin::current_year_month()),
                        "This month"
                    }
                }
            }
            p { class: "hint",
                if rows.is_empty() {
                    "Add a category below. A cap stays in place for every later month until you change it."
                } else if capped == 0 {
                    "Type a cap next to a category and press Enter. It applies from this month on. Leave it blank for no cap. Use the arrows to reorder. Untick In budget to keep a category out of caps and the month's spent."
                } else {
                    "{capped} of {rows.len()} capped. Changes apply from this month on. Blank means no cap. Use the arrows to reorder. Untick In budget to keep a category out of caps and the month's spent."
                }
            }

            table { class: "cats",
                if !rows.is_empty() {
                    thead {
                        tr {
                            th { class: "order", "" }
                            th { "Category" }
                            th { "Description" }
                            th { class: "flag", "In budget" }
                            th { class: "num", "Cap" }
                            th { class: "num", "Spent" }
                            th { "" }
                        }
                    }
                }
                tbody {
                    for (i, r) in rows.iter().enumerate() {
                        {
                            let id = r.category_id.clone();
                            let cat = cats.get(&id);
                            let is_child = r.parent_id.is_some();
                            let adding_here =
                                adding() && group_closed_at(&rows, i) == Some(new_parent().as_str());
                            let has_children = rows
                                .iter()
                                .any(|x| x.parent_id.as_deref() == Some(id.as_str())); // Arrows only move among siblings: top-level rows, or one parent's children.
                            let is_editing = editing().as_deref() == Some(id.as_str());
                            let armed = confirm_delete().as_deref() == Some(id.as_str());
                            let over = r.cap_cents > 0 && r.spent_cents > r.cap_cents;
                            let siblings: Vec<&str> = rows
                                .iter()
                                .filter(|x| x.parent_id == r.parent_id)
                                .map(|x| x.category_id.as_str())
                                .collect();
                            let first = siblings.first() == Some(&id.as_str());
                            let last = siblings.last() == Some(&id.as_str());
                            let parent_off = cat.map(|c| !c.parent_in_budget).unwrap_or(false);
                            let description = cat.and_then(|c| c.description.clone()).unwrap_or_default();
                            let mut class = String::from(if over { "over" } else { "" });
                            if is_child {
                                class.push_str(" child");
                            }
                            if !r.in_budget {
                                class.push_str(" off-budget");
                            }
                            rsx! {
                                tr { key: "{id}", class: "{class}",
                                    td { class: "order",
                                        button {
                                            class: "ghost small arrow",
                                            aria_label: "Move {r.category_name} up",
                                            title: "Move up",
                                            disabled: first,
                                            onclick: {
                                                let id = id.clone();
                                                move |_| move_cat(id.clone(), -1)
                                            },
                                            "▲"
                                        }
                                        button {
                                            class: "ghost small arrow",
                                            aria_label: "Move {r.category_name} down",
                                            title: "Move down",
                                            disabled: last,
                                            onclick: {
                                                let id = id.clone();
                                                move |_| move_cat(id.clone(), 1)
                                            },
                                            "▼"
                                        }
                                    }
                                    td { class: "name",
                                        if is_editing {
                                            div { class: "name-edit",
                                                input {
                                                    value: "{edit_name}",
                                                    aria_label: "Category name",
                                                    autofocus: true,
                                                    oninput: move |e| edit_name.set(e.value()),
                                                    onkeydown: {
                                                        let id = id.clone();
                                                        move |e| match e.key() {
                                                            Key::Enter => save_rename(id.clone()),
                                                            Key::Escape => editing.set(None),
                                                            _ => {}
                                                        }
                                                    },
                                                }
                                                if !has_children {
                                                    select {
                                                        class: "parent-select",
                                                        aria_label: "Parent of {r.category_name}",
                                                        onchange: {
                                                            let id = id.clone();
                                                            move |e| set_parent(id.clone(), e.value())
                                                        },
                                                        option { value: TOP_LEVEL, selected: !is_child, "Top level" }
                                                        for (pid, pname) in top_level.iter().filter(|(pid, _)| pid != &id) {
                                                            option {
                                                                value: "{pid}",
                                                                selected: r.parent_id.as_deref() == Some(pid.as_str()),
                                                                "Under {pname}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            span { class: "name-label",
                                                span { class: "cat-name", "{r.category_name}" }
                                                if !is_child {
                                                    button {
                                                        class: "ghost small add-sub",
                                                        aria_label: "Add a sub-category under {r.category_name}",
                                                        title: "Add a sub-category",
                                                        onclick: {
                                                            let id = id.clone();
                                                            move |_| open_add(id.clone())
                                                        },
                                                        "+"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    td {
                                        input {
                                            class: "desc-input",
                                            aria_label: "Description for {r.category_name}",
                                            placeholder: "What belongs here (used by AI)",
                                            value: "{description}",
                                            spellcheck: "false",
                                            onchange: {
                                                let id = id.clone();
                                                move |e| {
                                                    if let Some(s) = store() {
                                                        if let Err(err) = s
                                                            .lock()
                                                            .unwrap()
                                                            .set_category_description(&id, &e.value())
                                                        {
                                                            push_status(status, err.as_user_message());
                                                        }
                                                    }
                                                    super::bump(nonce);
                                                }
                                            },
                                        }
                                    }
                                    td { class: "flag",
                                        input {
                                            r#type: "checkbox",
                                            aria_label: "{r.category_name} in budget",
                                            title: if parent_off { "Its parent is off budget" } else { "Counts toward caps and the month's spent" },
                                            checked: r.in_budget,
                                            disabled: parent_off,
                                            onchange: {
                                                let id = id.clone();
                                                move |e| set_in_budget(id.clone(), e.checked())
                                            },
                                        }
                                    }
                                    td { class: "num",
                                        if r.in_budget {
                                            input {
                                                class: "num cap-input",
                                                aria_label: "Monthly cap for {r.category_name}",
                                                placeholder: "none",
                                                value: if r.cap_cents > 0 { format_cents(r.cap_cents) } else { String::new() },
                                                onchange: {
                                                    let id = id.clone();
                                                    move |e| {
                                                        let raw = e.value();
                                                        let cents = if raw.trim().is_empty() { Ok(0) } else { parse_cents(&raw) };
                                                        match cents {
                                                            Ok(c) if c < 0 => push_status(status, "A cap can't be negative."),
                                                            Ok(c) => {
                                                                if let Some(s) = store() {
                                                                    if let Err(err) = s
                                                                        .lock()
                                                                        .unwrap()
                                                                        .set_budget(&id, year, month, c)
                                                                    {
                                                                        push_status(status, err.as_user_message());
                                                                    }
                                                                }
                                                            }
                                                            Err(err) => push_status(status, err.as_user_message()),
                                                        }
                                                        super::bump(nonce);
                                                    }
                                                },
                                            }
                                        } else {
                                            span { class: "hint not-budgeted", "off budget" }
                                        }
                                    }
                                    td { class: "num spent", "${format_cents(r.spent_cents)}" }
                                    td { class: "actions",
                                        if is_editing {
                                            button {
                                                class: "primary small",
                                                onclick: {
                                                    let id = id.clone();
                                                    move |_| save_rename(id.clone())
                                                },
                                                "Save"
                                            }
                                            button { class: "ghost small", onclick: move |_| editing.set(None), "Cancel" }
                                        } else if armed {
                                            span { class: "hint",
                                                if has_children {
                                                    "Its sub-categories go too. Rows go uncategorized; caps and rules go away."
                                                } else {
                                                    "Rows go uncategorized; its caps and rules go away."
                                                }
                                            }
                                            button {
                                                class: "danger on small",
                                                onclick: {
                                                    let id = id.clone();
                                                    move |_| {
                                                        if let Some(s) = store() {
                                                            match s.lock().unwrap().delete_category(&id) {
                                                                Ok(_) => push_status(status, "Category deleted."),
                                                                Err(e) => push_status(status, e.as_user_message()),
                                                            }
                                                        }
                                                        confirm_delete.set(None);
                                                        super::bump(nonce);
                                                    }
                                                },
                                                "Confirm"
                                            }
                                            button {
                                                class: "ghost small",
                                                onclick: move |_| confirm_delete.set(None),
                                                "Cancel"
                                            }
                                        } else {
                                            button {
                                                class: "ghost small",
                                                onclick: {
                                                    let id = id.clone();
                                                    let name = r.category_name.clone();
                                                    move |_| {
                                                        editing.set(Some(id.clone()));
                                                        edit_name.set(name.clone());
                                                        confirm_delete.set(None);
                                                    }
                                                },
                                                "Rename"
                                            }
                                            button {
                                                class: "danger small",
                                                onclick: {
                                                    let id = id.clone();
                                                    move |_| {
                                                        confirm_delete.set(Some(id.clone()));
                                                        editing.set(None);
                                                    }
                                                },
                                                "Delete"
                                            }
                                        }
                                    }
                                }
                                if adding_here {
                                    tr { class: "add-row child",
                                        {add_input("New sub-category, e.g. Coffee")}
                                    }
                                }
                            }
                        }
                    }
                    tr { class: "add-row",
                        if adding() && new_parent() == TOP_LEVEL {
                            {add_input("New category, e.g. Groceries")}
                        } else {
                            td { colspan: "7",
                                button {
                                    class: "ghost small add-cat",
                                    onclick: move |_| open_add(TOP_LEVEL.to_string()),
                                    "+ Add"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, parent: Option<&str>) -> MonthRow {
        MonthRow {
            category_id: id.to_string(),
            category_name: id.to_string(),
            parent_id: parent.map(str::to_string),
            in_budget: true,
            cap_cents: 0,
            spent_cents: 0,
        }
    }

    #[test]
    fn sub_category_add_row_follows_the_parents_group() {
        let rows = vec![
            row("food", None),
            row("coffee", Some("food")),
            row("dining", Some("food")),
            row("gas", None),
        ];
        // A parent with children closes its group at the last child, not at itself.
        assert_eq!(group_closed_at(&rows, 0), None);
        assert_eq!(group_closed_at(&rows, 1), None);
        assert_eq!(group_closed_at(&rows, 2), Some("food"));
        // A childless parent closes its own group.
        assert_eq!(group_closed_at(&rows, 3), Some("gas"));
        assert_eq!(group_closed_at(&rows, 4), None);
    }
}
