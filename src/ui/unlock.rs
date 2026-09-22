use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dioxus::prelude::*;

use super::{activity_scope_query, screen_for, Screen};
use crate::SharedStore;
use myphin::Store;
use myphin::TxnQuery;

fn last_dir_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("myphin")
        .join("last_dir")
}

fn load_last_dir() -> String {
    std::fs::read_to_string(last_dir_path()).unwrap_or_default()
}

fn save_last_dir(dir: &str) {
    if let Some(parent) = last_dir_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(last_dir_path(), dir);
}

#[component]
pub fn Unlock(
    store: Signal<Option<SharedStore>>,
    screen: Signal<Screen>,
    filter: Signal<TxnQuery>,
) -> Element {
    let mut store = store;
    let mut screen = screen;
    let mut filter = filter;
    let mut dir = use_signal(load_last_dir);
    let mut passphrase = use_signal(|| String::new());
    let mut error = use_signal(|| None::<String>);

    let mut unlock = move |_| {
        error.set(None);
        if dir.read().trim().is_empty() {
            error.set(Some("Pick a folder.".into()));
            return;
        }
        let path = PathBuf::from(dir.read().trim());
        let opened = Store::open(&path, passphrase.read().as_str());
        match opened {
            Ok(s) => {
                save_last_dir(dir.read().trim());
                passphrase.set(String::new());
                // Apply before the shell paints, so the first frame is the saved screen.
                let screen_id = s
                    .open_screen()
                    .unwrap_or_else(|_| myphin::store::DEFAULT_OPEN_SCREEN.to_string());
                let scope = s
                    .activity_scope()
                    .unwrap_or_else(|_| myphin::store::DEFAULT_ACTIVITY_SCOPE.to_string());
                screen.set(screen_for(&screen_id));
                filter.set(activity_scope_query(&scope));
                store.set(Some(Arc::new(Mutex::new(s))));
            }
            Err(e) => error.set(Some(e.as_user_message())),
        }
    };

    rsx! {
        div { class: "unlock",
            div { class: "unlock-card",
                h1 { "Myphin" }
                p { class: "lede", "A ledger that stays in a folder you pick." }
                label { r#for: "unlock-dir", "Data folder" }
                div { class: "row",
                    input {
                        id: "unlock-dir",
                        value: "{dir}",
                        oninput: move |e| dir.set(e.value()),
                        placeholder: "/path/to/finance"
                    }
                    button {
                        class: "ghost",
                        onclick: move |_| {
                            if let Some(picked) = rfd::FileDialog::new().pick_folder() {
                                dir.set(picked.display().to_string());
                            }
                        },
                        "Browse"
                    }
                }
                label { r#for: "unlock-pass", "Passphrase" }
                input {
                    id: "unlock-pass",
                    r#type: "password",
                    value: "{passphrase}",
                    autofocus: !dir.read().is_empty(),
                    oninput: move |e| passphrase.set(e.value()),
                    onkeydown: move |e| if e.key() == Key::Enter { unlock(()) },
                }
                if let Some(err) = error.read().as_ref() {
                    p { class: "err", role: "alert", "{err}" }
                }
                button { class: "primary", onclick: move |_| unlock(()), "Unlock" }
                p { class: "hint",
                    "An empty folder becomes a new encrypted ledger. The passphrase is never stored, so there is no way to recover it."
                }
            }
        }
    }
}
