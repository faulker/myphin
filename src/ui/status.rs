//! Session status: a floating toast plus an append-only log.
//!
//! The toast's timeout runs in the webview: a CSS animation on its progress bar, sized from the
//! seconds saved in Setup → Log, that pauses while the mouse is over the toast and expires the
//! toast when it ends. Nothing here sleeps.

use crate::SharedStore;
use dioxus::prelude::*;
use myphin::TxnPatch;

/// Toasts with an Undo button stay this much longer so there is time to hit it.
const UNDO_EXTRA_SECS: u64 = 3;

/// How long a toast stays up: the configured base, plus a little more when it carries an Undo.
pub fn toast_secs(base_secs: u64, has_undo: bool) -> u64 {
    if has_undo {
        base_secs + UNDO_EXTRA_SECS
    } else {
        base_secs
    }
}

/// A change that can be reversed: the patch that puts these rows back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undo {
    pub ids: Vec<String>,
    pub patch: TxnPatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub message: String,
    pub undo: Option<Undo>,
}

/// One line of the session log. `undo` is taken once it is used, and `undone` marks that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub id: u64,
    pub message: String,
    pub undo: Option<Undo>,
    pub undone: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusState {
    pub toast: Option<Toast>,
    pub log: Vec<LogEntry>,
    next_id: u64,
}

impl StatusState {
    /// Record a message in the log and show it as the current toast. Returns the toast id.
    pub fn push(&mut self, message: String) -> u64 {
        self.push_with(message, None)
    }

    /// Like [`push`](Self::push), with a change the user can reverse from the toast or the log.
    pub fn push_with(&mut self, message: String, undo: Option<Undo>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.log.push(LogEntry {
            id,
            message: message.clone(),
            undo: undo.clone(),
            undone: false,
        });
        self.toast = Some(Toast { id, message, undo });
        id
    }

    /// Take the undo for entry `id`, marking it undone everywhere it shows. `None` if the
    /// entry has no undo or it was already used.
    pub fn take_undo(&mut self, id: u64) -> Option<Undo> {
        let entry = self.log.iter_mut().find(|e| e.id == id)?;
        let undo = entry.undo.take()?;
        entry.undone = true;
        if let Some(t) = self.toast.as_mut().filter(|t| t.id == id) {
            t.undo = None;
        }
        Some(undo)
    }

    /// Hide the toast without touching the log.
    pub fn dismiss(&mut self) {
        self.toast = None;
    }

    /// Hide the toast only if it is still this id (a newer push wins).
    pub fn expire(&mut self, id: u64) {
        if self.toast.as_ref().is_some_and(|t| t.id == id) {
            self.toast = None;
        }
    }
}

/// Push a status message. The toast hides itself when its timer bar runs out.
pub fn push_status(mut status: Signal<StatusState>, message: impl Into<String>) {
    status.write().push(message.into());
}

/// Push a status message for a change the user can undo from the toast or Setup → Log.
pub fn push_undoable(mut status: Signal<StatusState>, message: impl Into<String>, undo: Undo) {
    status.write().push_with(message.into(), Some(undo));
}

/// Reverse log entry `id`: apply its stored patch, then say so. Does nothing if it was
/// already undone.
pub fn undo_entry(
    store: Signal<Option<SharedStore>>,
    mut status: Signal<StatusState>,
    nonce: Signal<u64>,
    id: u64,
) {
    let Some(undo) = status.write().take_undo(id) else {
        return;
    };
    let Some(s) = store() else {
        return;
    };
    let result = s.lock().unwrap().patch_transactions(&undo.ids, &undo.patch);
    match result {
        Ok(_) => push_status(status, "Undone."),
        Err(e) => push_status(status, e.as_user_message()),
    }
    super::bump(nonce);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_appends_log_and_sets_toast() {
        let mut s = StatusState::default();
        let a = s.push("Syncing…".into());
        let b = s.push("Synced: 3 new.".into());
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        let msgs: Vec<&str> = s.log.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(msgs, ["Syncing…", "Synced: 3 new."]);
        assert_eq!(s.toast.as_ref().unwrap().message, "Synced: 3 new.");
        assert!(s.toast.as_ref().unwrap().undo.is_none());
    }

    #[test]
    fn take_undo_marks_entry_and_toast_once() {
        let mut s = StatusState::default();
        let undo = Undo {
            ids: vec!["a".into()],
            patch: TxnPatch {
                excluded: Some(false),
                ..Default::default()
            },
        };
        let plain = s.push("no undo".into());
        let id = s.push_with("Marked X as excluded.".into(), Some(undo.clone()));
        assert_eq!(s.toast.as_ref().unwrap().undo, Some(undo.clone()));
        assert_eq!(s.take_undo(plain), None);
        assert_eq!(s.take_undo(id), Some(undo));
        assert_eq!(s.take_undo(id), None);
        let entry = s.log.iter().find(|e| e.id == id).unwrap();
        assert!(entry.undone);
        assert!(entry.undo.is_none());
        assert!(s.toast.as_ref().unwrap().undo.is_none());
        assert_eq!(s.take_undo(99), None);
    }

    #[test]
    fn undo_toasts_stay_longer() {
        assert_eq!(toast_secs(5, false), 5);
        assert_eq!(toast_secs(5, true), 8);
        assert_eq!(toast_secs(20, true), 23);
    }

    #[test]
    fn expire_ignores_stale_id() {
        let mut s = StatusState::default();
        let first = s.push("one".into());
        s.push("two".into());
        s.expire(first);
        assert_eq!(s.toast.as_ref().unwrap().message, "two");
        s.expire(2);
        assert!(s.toast.is_none());
        assert_eq!(s.log.len(), 2);
    }

    #[test]
    fn dismiss_clears_toast_keeps_log() {
        let mut s = StatusState::default();
        s.push("kept".into());
        s.dismiss();
        assert!(s.toast.is_none());
        assert_eq!(s.log.len(), 1);
        assert_eq!(s.log[0].message, "kept");
    }
}
