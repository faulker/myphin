//! Categories, budgets, rules, splits, transfers, bulk edits.

use std::collections::HashMap;

use chrono::{Datelike, TimeZone, Utc};
use regex::Regex;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::ai::AiExchange;
use crate::error::Error;
use crate::store::Store;

/// Processor / POS prefixes, longest first.
const PROCESSOR_PREFIXES: &[&str] = &[
    "amazon.com*",
    "amzn mktp",
    "paypal *",
    "checkcard",
    "pos debit",
    "sq *",
    "tst*",
    "sp *",
    "pp *",
];

/// Collapse bank noise into a stable payee memory key.
pub fn normalize_payee(raw: &str) -> String {
    let mut s = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    loop {
        let mut hit = false;
        for prefix in PROCESSOR_PREFIXES {
            if s.starts_with(prefix) {
                s = s[prefix.len()..].trim_start().to_string();
                hit = true;
                break;
            }
        }
        if !hit {
            break;
        }
    }
    let mut cleaned = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            let mut ate = false;
            while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                chars.next();
                ate = true;
            }
            if ate {
                cleaned.push(' ');
                continue;
            }
        }
        cleaned.push(c);
    }
    s = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens
        .first()
        .is_some_and(|t| t.chars().all(|c| c.is_ascii_digit()))
    {
        tokens.remove(0);
    }
    if tokens.last().is_some_and(|t| is_us_state(t)) {
        tokens.pop();
    }
    tokens.join(" ")
}

/// True for a two-letter US state or DC abbreviation.
fn is_us_state(tok: &str) -> bool {
    matches!(
        tok,
        "al" | "ak"
            | "az"
            | "ar"
            | "ca"
            | "co"
            | "ct"
            | "de"
            | "fl"
            | "ga"
            | "hi"
            | "id"
            | "il"
            | "in"
            | "ia"
            | "ks"
            | "ky"
            | "la"
            | "me"
            | "md"
            | "ma"
            | "mi"
            | "mn"
            | "ms"
            | "mo"
            | "mt"
            | "ne"
            | "nv"
            | "nh"
            | "nj"
            | "nm"
            | "ny"
            | "nc"
            | "nd"
            | "oh"
            | "ok"
            | "or"
            | "pa"
            | "ri"
            | "sc"
            | "sd"
            | "tn"
            | "tx"
            | "ut"
            | "vt"
            | "va"
            | "wa"
            | "wv"
            | "wi"
            | "wy"
            | "dc"
    )
}

/// Joins a parent name and a sub-category name in labels, e.g. `Investment › Fees`.
pub const CATEGORY_SEP: &str = " › ";

/// SQL for `Category::label()` where the category is aliased `c` and its parent `pc`.
const CATEGORY_LABEL_SQL: &str =
    "CASE WHEN pc.name IS NULL THEN c.name ELSE pc.name || ' › ' || c.name END";

#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub id: String,
    pub name: String,
    /// What belongs here. Sent to the AI categorizer as context for this option.
    pub description: Option<String>,
    /// The top-level category this one sits under. Nesting is one level deep.
    pub parent_id: Option<String>,
    /// The parent's name, for labels. `None` at top level.
    pub parent_name: Option<String>,
    /// This row's own flag. See `is_budgeted` for the value that counts.
    pub in_budget: bool,
    /// The parent's flag, `true` at top level. A child under an off-budget parent is off too.
    pub parent_in_budget: bool,
    /// This row's own flag. See `is_sent_to_ai` for the value that counts. Independent of
    /// `in_budget`: a catch-all can stay in the budget but off the list the model sees.
    pub send_to_ai: bool,
    /// The parent's flag, `true` at top level. A child under a parent left off the list is off too.
    pub parent_send_to_ai: bool,
}

impl Category {
    /// `Parent › Child` for a sub-category, the plain name otherwise.
    pub fn label(&self) -> String {
        match &self.parent_name {
            Some(p) => format!("{p}{CATEGORY_SEP}{}", self.name),
            None => self.name.clone(),
        }
    }

    /// Whether spend here counts toward the budget: the row and its parent are both in.
    pub fn is_budgeted(&self) -> bool {
        self.in_budget && self.parent_in_budget
    }

    /// Whether the AI categorizer sees this category: the row and its parent are both on.
    pub fn is_sent_to_ai(&self) -> bool {
        self.send_to_ai && self.parent_send_to_ai
    }
}

/// Unique category for typed Activity input. In order, each taken only when it names exactly
/// one category: exact name, exact label, unique name prefix, unique label prefix. Name
/// before label keeps "Inv" landing on "Investment" even when "Investment › Fees" exists.
pub fn match_category_typeahead<'a>(typed: &str, cats: &'a [Category]) -> Option<&'a Category> {
    let q = typed.trim().to_ascii_lowercase();
    if q.is_empty() {
        return None;
    }
    let passes: [&dyn Fn(&Category) -> bool; 4] = [
        &|c| c.name.to_ascii_lowercase() == q,
        &|c| c.label().to_ascii_lowercase() == q,
        &|c| c.name.to_ascii_lowercase().starts_with(&q),
        &|c| c.label().to_ascii_lowercase().starts_with(&q),
    ];
    for pass in passes {
        let mut hits = cats.iter().filter(|c| pass(c));
        if let Some(first) = hits.next() {
            return if hits.next().is_some() {
                None
            } else {
                Some(first)
            };
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub struct Txn {
    pub id: String,
    pub account_id: String,
    pub account_name: String,
    pub posted_at: i64,
    pub amount_cents: i64,
    pub payee: String,
    pub notes: Option<String>,
    pub category_id: Option<String>,
    pub category_name: Option<String>,
    pub excluded: bool,
    pub pending: bool,
    pub is_transfer: bool,
    /// Which kind of transfer this is, when it is a credit card or loan payment. Always a
    /// transfer too, so it is ignored by caps and spend the same way.
    pub payment: Option<PaymentKind>,
    pub is_income: bool,
    pub has_splits: bool,
    pub parent_id: Option<String>,
    /// `user`, `rule`, `memory`, or `ai`. `None` when nothing set the category.
    pub categorized_by: Option<String>,
    /// When the last AI attempt on this row errored. Such rows are skipped by the after-sync
    /// pass and the on-screen bulk button until "Ask AI" or Setup → AI retries them.
    pub ai_failed_at: Option<i64>,
}

/// One remembered AI answer, with the round trips that produced it, for the debug view.
#[derive(Debug, Clone, PartialEq)]
pub struct AiRecord {
    /// The payee text as the AI pass would send it today.
    pub payee: String,
    /// `None` means the provider said nothing fits.
    pub category_id: Option<String>,
    /// Looked up now; `None` when the category is gone or the answer was "other".
    pub category_name: Option<String>,
    pub confidence: f64,
    pub provider: String,
    pub asked_at: i64,
    /// Empty for answers recorded before exchanges were kept.
    pub exchanges: Vec<AiExchange>,
    /// Set when the last attempt failed: the sanitized provider error. `category_id` and
    /// `confidence` mean nothing then, and the payee is asked again on the next run.
    pub error: Option<String>,
}

/// A transfer that pays down debt: a credit card bill or a loan installment. Labelled apart
/// from plain transfers, but ignored by the budget the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentKind {
    Card,
    Loan,
}

impl PaymentKind {
    /// Stored form in the `transactions.transfer_kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            PaymentKind::Card => "card",
            PaymentKind::Loan => "loan",
        }
    }

    /// Parse the stored form. Anything else is a plain transfer.
    pub fn parse(s: &str) -> Option<PaymentKind> {
        match s {
            "card" => Some(PaymentKind::Card),
            "loan" => Some(PaymentKind::Loan),
            _ => None,
        }
    }

    /// Short label for row tags.
    pub fn label(self) -> &'static str {
        match self {
            PaymentKind::Card => "card payment",
            PaymentKind::Loan => "loan payment",
        }
    }
}

/// One bank account, as Setup and the Activity account filter list them.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub connection_id: String,
    pub name: String,
    /// The bank the account lives at, when the source said. Shown beside the name because
    /// an aggregator connection like SimpleFIN can hold accounts from several banks.
    pub institution: Option<String>,
    /// Hidden accounts drop out of Activity, counts, and caps unless filtered for explicitly.
    pub hidden: bool,
}

impl Account {
    /// The name with its bank, "Checking · Example Bank", or just the name when the bank
    /// is unknown or already part of the name.
    pub fn label(&self) -> String {
        match &self.institution {
            Some(bank) if !self.name.to_lowercase().contains(&bank.to_lowercase()) => {
                format!("{} · {}", self.name, bank)
            }
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxnPatch {
    pub category_id: Option<Option<String>>,
    pub payee: Option<String>,
    pub notes: Option<Option<String>>,
    pub excluded: Option<bool>,
    pub amount_cents: Option<i64>,
    pub posted_at: Option<i64>,
    pub transfer_account_id: Option<Option<String>>,
    /// `Some(Some(kind))` marks the row as that payment (and as a transfer, if it is not one
    /// yet); `Some(None)` drops the kind but leaves the transfer flag alone.
    pub payment: Option<Option<PaymentKind>>,
    pub income: Option<bool>,
}

impl TxnPatch {
    /// The patch that puts `txn` back after this one is applied: every field this patch sets
    /// is filled with the row's current value. Transfer flag and payment kind travel together,
    /// since the store derives one from the other.
    pub fn reverse_for(&self, txn: &Txn) -> TxnPatch {
        let touches_transfer = self.transfer_account_id.is_some() || self.payment.is_some();
        TxnPatch {
            category_id: self.category_id.as_ref().map(|_| txn.category_id.clone()),
            payee: self.payee.as_ref().map(|_| txn.payee.clone()),
            notes: self.notes.as_ref().map(|_| txn.notes.clone()),
            excluded: self.excluded.map(|_| txn.excluded),
            amount_cents: self.amount_cents.map(|_| txn.amount_cents),
            posted_at: self.posted_at.map(|_| txn.posted_at),
            transfer_account_id: touches_transfer
                .then(|| txn.is_transfer.then(|| "manual".to_string())),
            payment: touches_transfer.then_some(txn.payment),
            income: self.income.map(|_| txn.is_income),
        }
    }
}

/// What a matching rule does to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// Set the rule's category.
    Category,
    /// Mark as a transfer (no category needed, skips caps).
    Transfer,
    /// Mark as a credit card payment (a transfer, skips caps).
    CardPayment,
    /// Mark as a loan payment (a transfer, skips caps).
    LoanPayment,
    /// Mark as income / paycheck.
    Income,
    /// Exclude from everything.
    Exclude,
}

impl RuleAction {
    /// Stored form in the `rules.action` column.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleAction::Category => "category",
            RuleAction::Transfer => "transfer",
            RuleAction::CardPayment => "card_payment",
            RuleAction::LoanPayment => "loan_payment",
            RuleAction::Income => "income",
            RuleAction::Exclude => "exclude",
        }
    }

    /// Parse the stored form. Unknown values fall back to `Category`.
    pub fn parse(s: &str) -> RuleAction {
        match s {
            "transfer" => RuleAction::Transfer,
            "card_payment" => RuleAction::CardPayment,
            "loan_payment" => RuleAction::LoanPayment,
            "income" => RuleAction::Income,
            "exclude" => RuleAction::Exclude,
            _ => RuleAction::Category,
        }
    }

    /// Short label for rule lists.
    pub fn label(self) -> &'static str {
        match self {
            RuleAction::Category => "categorize",
            RuleAction::Transfer => "mark transfer",
            RuleAction::CardPayment => "mark card payment",
            RuleAction::LoanPayment => "mark loan payment",
            RuleAction::Income => "mark income",
            RuleAction::Exclude => "exclude",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub priority: i64,
    /// One or more wildcard patterns separated by `|`, each matched against the whole
    /// description; any one matching is enough. See [`pattern_match`].
    pub description_pattern: Option<String>,
    pub description_regex: Option<String>,
    pub amount_min_cents: Option<i64>,
    pub amount_max_cents: Option<i64>,
    pub account_id: Option<String>,
    pub action: RuleAction,
    /// Required when `action` is `Category`, ignored otherwise.
    pub category_id: Option<String>,
    pub enabled: bool,
}

impl Rule {
    /// A "description matches pattern" rule with the given action. `pattern` may hold several
    /// alternatives separated by `|`. Priority is set by the caller.
    pub fn pattern(
        pattern: &str,
        action: RuleAction,
        category_id: Option<String>,
        priority: i64,
    ) -> Rule {
        Rule {
            id: String::new(),
            priority,
            description_pattern: Some(pattern.to_string()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action,
            category_id,
            enabled: true,
        }
    }
}

/// One category's month, in `list_categories` order (a parent directly before its children).
#[derive(Debug, Clone)]
pub struct MonthRow {
    pub category_id: String,
    pub category_name: String,
    pub parent_id: Option<String>,
    /// Whether the row counts toward the budget (its own flag and its parent's).
    pub in_budget: bool,
    /// 0 for no cap, and always 0 when the row is off budget.
    pub cap_cents: i64,
    /// A leaf's own spend. A parent's own spend plus its budgeted children's.
    pub spent_cents: i64,
}

/// Filters for the Activity list. Every field is optional; the default lists everything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TxnQuery {
    pub uncategorized_only: bool,
    /// Only rows whose category came from the AI pass (the review list).
    pub ai_only: bool,
    /// Only rows marked excluded, so they can be found and included again.
    pub excluded_only: bool,
    pub category_id: Option<String>,
    pub account_id: Option<String>,
    pub month: Option<(i32, u32)>,
    /// Case-insensitive match on payee or notes. If it parses as an amount, matches that amount too.
    pub search: String,
}

impl Store {
    /// Insert a top-level category. Rejects blank names.
    pub fn add_category(&self, name: &str) -> Result<String, Error> {
        self.insert_category(name, None)
    }

    /// Insert a category under `parent_id`. The parent must be top-level: nesting stops at
    /// one level. Rejects blank names.
    pub fn add_subcategory(&self, parent_id: &str, name: &str) -> Result<String, Error> {
        self.require_top_level_parent(parent_id)?;
        self.insert_category(name, Some(parent_id))
    }

    fn insert_category(&self, name: &str, parent_id: Option<&str>) -> Result<String, Error> {
        let name = require_category_name(name)?;
        let id = Uuid::new_v4().to_string();
        let max: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(sort_order),0) FROM categories WHERE parent_id IS ?1",
            params![parent_id],
            |r| r.get(0),
        )?;
        self.conn().execute(
            "INSERT INTO categories (id, name, sort_order, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, max + 1, parent_id],
        )?;
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(id)
    }

    /// `parent_id` of a category: `None` when the row is gone, `Some(None)` at top level.
    fn category_parent(&self, id: &str) -> Result<Option<Option<String>>, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT parent_id FROM categories WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Error unless `parent_id` names a category that is itself top-level.
    fn require_top_level_parent(&self, parent_id: &str) -> Result<(), Error> {
        match self.category_parent(parent_id)? {
            None => Err(Error::user("That category is gone.")),
            Some(Some(_)) => Err(Error::user(
                "Sub-categories can't have their own sub-categories.",
            )),
            Some(None) => Ok(()),
        }
    }

    fn child_category_ids(&self, id: &str) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .conn()
            .prepare("SELECT id FROM categories WHERE parent_id=?1 ORDER BY sort_order, name")?;
        let rows = stmt.query_map(params![id], |r| r.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Move a category under a top-level parent, or back to top level with `None`. A category
    /// that has sub-categories of its own stays put. It lands at the end of its new siblings.
    pub fn set_category_parent(&self, id: &str, parent_id: Option<&str>) -> Result<(), Error> {
        if self.category_parent(id)?.is_none() {
            return Err(Error::user("That category is gone."));
        }
        if let Some(pid) = parent_id {
            if pid == id {
                return Err(Error::user("A category can't sit under itself."));
            }
            self.require_top_level_parent(pid)?;
        }
        if !self.child_category_ids(id)?.is_empty() {
            return Err(Error::user("Move its sub-categories out first."));
        }
        let max: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(sort_order),0) FROM categories WHERE parent_id IS ?1",
            params![parent_id],
            |r| r.get(0),
        )?;
        self.conn().execute(
            "UPDATE categories SET parent_id=?2, sort_order=?3 WHERE id=?1",
            params![id, parent_id, max + 1],
        )?;
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(())
    }

    /// Put a category in or out of the budget. Off-budget rows keep taking transactions but
    /// have no cap and never count toward the month's spent. Turning a parent off also
    /// turns its sub-categories off, and a child can't be switched on while its parent is off.
    pub fn set_category_in_budget(&self, id: &str, on: bool) -> Result<(), Error> {
        let Some(parent) = self.category_parent(id)? else {
            return Err(Error::user("That category is gone."));
        };
        if on {
            if let Some(pid) = parent {
                let parent_on: i64 = self.conn().query_row(
                    "SELECT in_budget FROM categories WHERE id=?1",
                    params![pid],
                    |r| r.get(0),
                )?;
                if parent_on == 0 {
                    return Err(Error::user("Turn on its parent first."));
                }
            }
        }
        self.conn().execute(
            "UPDATE categories SET in_budget=?2 WHERE id=?1",
            params![id, on as i64],
        )?;
        if !on {
            self.conn().execute(
                "UPDATE categories SET in_budget=0 WHERE parent_id=?1",
                params![id],
            )?;
        }
        self.persist()?;
        Ok(())
    }

    /// Include or omit a category from the option list sent to the AI categorizer. Changing
    /// it clears cached answers, because the list the model saw changed. Turning a parent
    /// off also turns its sub-categories off, and a child can't be switched on while its
    /// parent is off.
    pub fn set_category_send_to_ai(&self, id: &str, on: bool) -> Result<(), Error> {
        let Some(parent) = self.category_parent(id)? else {
            return Err(Error::user("That category is gone."));
        };
        if on {
            if let Some(pid) = parent {
                let parent_on: i64 = self.conn().query_row(
                    "SELECT send_to_ai FROM categories WHERE id=?1",
                    params![pid],
                    |r| r.get(0),
                )?;
                if parent_on == 0 {
                    return Err(Error::user("Turn on its parent first."));
                }
            }
        }
        self.conn().execute(
            "UPDATE categories SET send_to_ai=?2 WHERE id=?1",
            params![id, on as i64],
        )?;
        if !on {
            self.conn().execute(
                "UPDATE categories SET send_to_ai=0 WHERE parent_id=?1",
                params![id],
            )?;
        }
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(())
    }

    /// Set (or blank) the description the AI categorizer sees for a category.
    pub fn set_category_description(&self, id: &str, description: &str) -> Result<(), Error> {
        let text = description.trim();
        let value = if text.is_empty() { None } else { Some(text) };
        let n = self.conn().execute(
            "UPDATE categories SET description=?2 WHERE id=?1",
            params![id, value],
        )?;
        if n == 0 {
            return Err(Error::user("That category is gone."));
        }
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(())
    }

    /// Forget cached AI answers. Called whenever the category list the model saw changes.
    fn clear_ai_answers(&self) -> Result<(), Error> {
        self.conn().execute("DELETE FROM ai_answers", [])?;
        Ok(())
    }

    /// Rename a category. Rejects blank names.
    pub fn rename_category(&self, id: &str, name: &str) -> Result<(), Error> {
        let name = require_category_name(name)?;
        let n = self.conn().execute(
            "UPDATE categories SET name=?2 WHERE id=?1",
            params![id, name],
        )?;
        if n == 0 {
            return Err(Error::user("That category is gone."));
        }
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(())
    }

    /// Delete a category and its sub-categories. Their transactions become uncategorized.
    /// Caps, rules, and payee memory for them go away.
    pub fn delete_category(&self, id: &str) -> Result<(), Error> {
        let tx = self.conn().unchecked_transaction()?;
        // Children first: they reference the parent and foreign keys are on.
        for child in self.child_category_ids(id)? {
            self.purge_category(&child)?;
        }
        let n = self.purge_category(id)?;
        tx.commit()?;
        if n == 0 {
            return Err(Error::user("That category is gone."));
        }
        self.clear_ai_answers()?;
        self.persist()?;
        Ok(())
    }

    /// Remove one category row and everything that points at it. Returns rows deleted from
    /// `categories` (0 when it was already gone).
    fn purge_category(&self, id: &str) -> Result<usize, Error> {
        self.conn().execute(
            "UPDATE transactions SET category_id=NULL, categorized_by=NULL WHERE category_id=?1",
            params![id],
        )?;
        self.conn()
            .execute("DELETE FROM budgets WHERE category_id=?1", params![id])?;
        self.conn()
            .execute("DELETE FROM rules WHERE category_id=?1", params![id])?;
        self.conn()
            .execute("DELETE FROM payee_memory WHERE category_id=?1", params![id])?;
        Ok(self
            .conn()
            .execute("DELETE FROM categories WHERE id=?1", params![id])?)
    }

    /// Move a category one step up (`delta = -1`) or down (`delta = 1`) among its siblings:
    /// the top-level list, or the children of one parent. Then rewrites every `sort_order`
    /// as 1..n in tree order so the order stays explicit even for ledgers that never set
    /// one. Returns `false` when the category is already at that end.
    pub fn move_category(&self, id: &str, delta: i32) -> Result<bool, Error> {
        let cats = self.list_categories()?;
        let Some(me) = cats.iter().find(|c| c.id == id) else {
            return Err(Error::user("That category is gone."));
        };
        let parent = me.parent_id.clone();
        let mut top: Vec<String> = Vec::new();
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        for c in &cats {
            match &c.parent_id {
                Some(p) => kids.entry(p.clone()).or_default().push(c.id.clone()),
                None => top.push(c.id.clone()),
            }
        }
        let siblings = match &parent {
            Some(p) => kids.get_mut(p).expect("parent listed"),
            None => &mut top,
        };
        let from = siblings.iter().position(|c| c == id).expect("listed");
        let to = from as i64 + delta as i64;
        if to < 0 || to >= siblings.len() as i64 {
            return Ok(false);
        }
        siblings.swap(from, to as usize);
        let mut order = 0i64;
        for pid in &top {
            for cid in std::iter::once(pid).chain(kids.get(pid).into_iter().flatten()) {
                order += 1;
                self.conn().execute(
                    "UPDATE categories SET sort_order=?2 WHERE id=?1",
                    params![cid, order],
                )?;
            }
        }
        self.persist()?;
        Ok(true)
    }

    /// Every category, each parent directly followed by its children.
    pub fn list_categories(&self) -> Result<Vec<Category>, Error> {
        let mut stmt = self.conn().prepare(
            "SELECT c.id, c.name, c.description, c.parent_id, p.name, c.in_budget,
                    COALESCE(p.in_budget, 1), c.send_to_ai, COALESCE(p.send_to_ai, 1)
             FROM categories c LEFT JOIN categories p ON p.id = c.parent_id
             ORDER BY COALESCE(p.sort_order, c.sort_order), COALESCE(p.name, c.name),
                      c.parent_id IS NOT NULL, c.sort_order, c.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Category {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                parent_id: r.get(3)?,
                parent_name: r.get(4)?,
                in_budget: r.get::<_, i64>(5)? != 0,
                parent_in_budget: r.get::<_, i64>(6)? != 0,
                send_to_ai: r.get::<_, i64>(7)? != 0,
                parent_send_to_ai: r.get::<_, i64>(8)? != 0,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Set a category's cap starting this month. It stays in force for every later month
    /// until another cap is set, so there is nothing to copy month to month. A cap of 0
    /// means "no cap" from this month on.
    pub fn set_budget(
        &self,
        category_id: &str,
        year: i32,
        month: u32,
        cap_cents: i64,
    ) -> Result<(), Error> {
        self.conn().execute(
            "INSERT INTO budgets (category_id, year, month, cap_cents) VALUES (?1,?2,?3,?4)
             ON CONFLICT(category_id, year, month) DO UPDATE SET cap_cents=excluded.cap_cents",
            params![category_id, year, month as i64, cap_cents],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn add_rule(&self, rule: &Rule) -> Result<String, Error> {
        let id = if rule.id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            rule.id.clone()
        };
        let (description_pattern, category_id) = validate_rule(rule)?;
        self.conn().execute(
            "INSERT INTO rules (id, priority, description_pattern, description_regex, amount_min_cents, amount_max_cents, account_id, category_id, enabled, action)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                rule.priority,
                description_pattern,
                rule.description_regex,
                rule.amount_min_cents,
                rule.amount_max_cents,
                rule.account_id,
                category_id,
                rule.enabled as i64,
                rule.action.as_str()
            ],
        )?;
        self.persist()?;
        Ok(id)
    }

    /// Add a rule and apply it to every existing row it matches, hand-categorized rows
    /// included. Earlier rules still win: a row an earlier rule already claims is left alone.
    /// Returns the new rule's id and how many rows changed.
    pub fn create_rule(&self, rule: &Rule) -> Result<(String, u32), Error> {
        let id = self.add_rule(rule)?;
        let n = self.apply_rule_to_existing(&id)?;
        Ok((id, n))
    }

    /// Change an existing rule's pattern, action, and category, then apply it to every row it
    /// now matches, like [`Store::create_rule`]. Rows the old pattern matched are left as they
    /// are. Priority and the other filters are kept. Returns how many rows changed.
    pub fn update_rule(
        &self,
        id: &str,
        pattern: &str,
        action: RuleAction,
        category_id: Option<String>,
    ) -> Result<u32, Error> {
        let (description_pattern, category_id) =
            validate_rule(&Rule::pattern(pattern, action, category_id, 0))?;
        let n = self.conn().execute(
            "UPDATE rules SET description_pattern=?2, action=?3, category_id=?4 WHERE id=?1",
            params![id, description_pattern, action.as_str(), category_id],
        )?;
        if n == 0 {
            return Err(Error::user("That rule is gone."));
        }
        self.apply_rule_to_existing(id)
    }

    /// Add the alternatives in `pattern` to rule `id`'s existing pattern, then apply the rule to
    /// every row it now matches, like [`Store::update_rule`]. Action, category, and position stay
    /// as they were. Alternatives the rule already has are not repeated (`’` and `'` count as
    /// the same). Returns how many rows changed.
    pub fn add_rule_pattern(&self, id: &str, pattern: &str) -> Result<u32, Error> {
        let rules = self.list_rules()?;
        let Some(rule) = rules.iter().find(|r| r.id == id) else {
            return Err(Error::user("That rule is gone."));
        };
        if split_patterns(pattern).next().is_none() {
            return Err(Error::user("A rule needs a pattern."));
        }
        let existing = rule.description_pattern.clone().unwrap_or_default();
        let mut alternatives: Vec<&str> = split_patterns(&existing).collect();
        for p in split_patterns(pattern) {
            if !alternatives
                .iter()
                .any(|a| straighten_quotes(a).eq_ignore_ascii_case(&straighten_quotes(p)))
            {
                alternatives.push(p);
            }
        }
        let joined = straighten_quotes(&alternatives.join(" | "));
        self.conn().execute(
            "UPDATE rules SET description_pattern=?2 WHERE id=?1",
            params![id, joined],
        )?;
        self.apply_rule_to_existing(id)
    }

    /// Run rule `id` over every live top-level row and apply it where it is the winning rule.
    /// Returns how many rows changed.
    fn apply_rule_to_existing(&self, id: &str) -> Result<u32, Error> {
        let rules = self.list_rules()?;
        let Some(new_rule) = rules.iter().find(|r| r.id == id) else {
            return Err(Error::user("That rule is gone."));
        };
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, COALESCE(user_payee, description), COALESCE(user_amount_cents, amount_cents), category_id
             FROM transactions
             WHERE deleted=0 AND parent_id IS NULL",
        )?;
        let rows: Vec<(String, String, String, i64, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut n = 0u32;
        for (txn_id, account_id, payee, amount, prev) in rows {
            let winner = match_rules(&rules, &payee, amount, &account_id);
            if winner.is_some_and(|w| w.id == id)
                && self.apply_rule_action(new_rule, &txn_id, prev.as_deref())?
            {
                n += 1;
            }
        }
        self.persist()?;
        Ok(n)
    }

    pub fn list_rules(&self) -> Result<Vec<Rule>, Error> {
        let mut stmt = self.conn().prepare(
            "SELECT id, priority, description_pattern, description_regex, amount_min_cents, amount_max_cents, account_id, category_id, enabled, action
             FROM rules ORDER BY priority ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Rule {
                id: r.get(0)?,
                priority: r.get(1)?,
                description_pattern: r.get(2)?,
                description_regex: r.get(3)?,
                amount_min_cents: r.get(4)?,
                amount_max_cents: r.get(5)?,
                account_id: r.get(6)?,
                category_id: r.get(7)?,
                enabled: r.get::<_, i64>(8)? != 0,
                action: RuleAction::parse(&r.get::<_, String>(9)?),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delete one auto-categorize rule.
    pub fn delete_rule(&self, id: &str) -> Result<(), Error> {
        let n = self
            .conn()
            .execute("DELETE FROM rules WHERE id=?1", params![id])?;
        if n == 0 {
            return Err(Error::user("That rule is gone."));
        }
        self.persist()?;
        Ok(())
    }

    /// Every account, hidden ones included, by name.
    /// Transactions whose description matches any alternative in `pattern`, newest first, so the
    /// Rules form can show what a rule would apply to before it is added. Hidden accounts are
    /// left out, like Activity.
    pub fn preview_rule(&self, pattern: &str) -> Result<Vec<Txn>, Error> {
        if split_patterns(pattern).next().is_none() {
            return Ok(Vec::new());
        }
        let rows = self.query_transactions(&TxnQuery::default())?;
        Ok(rows
            .into_iter()
            .filter(|t| pattern_match(pattern, &t.payee))
            .collect())
    }

    /// When `pattern` matches nothing but is probably meant as "contains", the form with every
    /// alternative wrapped in `*` and how many rows it would match. `None` when the pattern
    /// already matches rows, every alternative already has `*` at both ends, or the contains
    /// form matches nothing either.
    pub fn suggest_contains_pattern(
        &self,
        pattern: &str,
    ) -> Result<Option<(String, usize)>, Error> {
        let alternatives: Vec<&str> = split_patterns(pattern).collect();
        let cores: Vec<&str> = alternatives
            .iter()
            .map(|a| a.trim_matches('*'))
            .filter(|c| !c.is_empty())
            .collect();
        if cores.is_empty()
            || alternatives
                .iter()
                .all(|a| a.starts_with('*') && a.ends_with('*'))
        {
            return Ok(None);
        }
        if !self.preview_rule(pattern)?.is_empty() {
            return Ok(None);
        }
        let wrapped = cores
            .iter()
            .map(|c| format!("*{c}*"))
            .collect::<Vec<_>>()
            .join(" | ");
        let n = self.preview_rule(&wrapped)?.len();
        Ok((n > 0).then_some((wrapped, n)))
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, Error> {
        let mut stmt = self.conn().prepare(
            "SELECT id, connection_id, name, institution, hidden FROM accounts ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                connection_id: r.get(1)?,
                name: r.get(2)?,
                institution: r.get(3)?,
                hidden: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Hide or show every transaction of one account. Hidden accounts leave Activity, the
    /// uncategorized count, and category caps, but their rows stay in the ledger.
    pub fn set_account_hidden(&self, account_id: &str, hidden: bool) -> Result<(), Error> {
        let n = self.conn().execute(
            "UPDATE accounts SET hidden=?2 WHERE id=?1",
            params![account_id, hidden as i64],
        )?;
        if n == 0 {
            return Err(Error::user("That account is gone."));
        }
        self.persist()?;
        Ok(())
    }

    /// Uncategorized and All, as the Activity chips used them. See [`Store::query_transactions`].
    pub fn list_transactions(
        &self,
        uncategorized_only: bool,
        category_id: Option<&str>,
    ) -> Result<Vec<Txn>, Error> {
        self.query_transactions(&TxnQuery {
            uncategorized_only,
            category_id: category_id.map(str::to_string),
            ..Default::default()
        })
    }

    /// Top-level transactions matching `q`, newest first. Splits are folded into their parent.
    pub fn query_transactions(&self, q: &TxnQuery) -> Result<Vec<Txn>, Error> {
        let mut sql = String::from(
            "SELECT t.id, t.account_id, a.name,
                    COALESCE(t.user_posted_at, t.posted_at),
                    COALESCE(t.user_amount_cents, t.amount_cents),
                    COALESCE(t.user_payee, t.description),
                    t.notes, t.category_id, ",
        );
        sql.push_str(CATEGORY_LABEL_SQL);
        sql.push_str(
            ",
                    t.excluded, t.pending,
                    CASE WHEN t.transfer_peer_id IS NOT NULL OR t.transfer_account_id IS NOT NULL THEN 1 ELSE 0 END,
                    (SELECT COUNT(*) FROM transactions s WHERE s.parent_id = t.id AND s.deleted=0),
                    t.parent_id, t.is_income, t.transfer_kind, t.categorized_by, t.ai_failed_at
             FROM transactions t
             JOIN accounts a ON a.id = t.account_id
             LEFT JOIN categories c ON c.id = t.category_id
             LEFT JOIN categories pc ON pc.id = c.parent_id
             WHERE t.deleted=0 AND t.parent_id IS NULL ",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if q.uncategorized_only {
            sql.push_str(" AND t.category_id IS NULL AND t.excluded=0 AND t.is_income=0 AND t.transfer_peer_id IS NULL AND t.transfer_account_id IS NULL ");
        }
        if q.ai_only {
            sql.push_str(" AND t.categorized_by='ai' ");
        }
        if q.excluded_only {
            sql.push_str(" AND t.excluded=1 ");
        }
        // A parent covers its sub-categories' rows too.
        if let Some(cid) = &q.category_id {
            args.push(Box::new(cid.clone()));
            let n = args.len();
            sql.push_str(&format!(
                " AND (t.category_id = ?{n} OR t.category_id IN (SELECT id FROM categories WHERE parent_id = ?{n})) "
            ));
        }
        // Picking an account in the filter is the one way to see a hidden account's rows.
        if let Some(aid) = &q.account_id {
            args.push(Box::new(aid.clone()));
            sql.push_str(&format!(" AND t.account_id = ?{} ", args.len()));
        } else {
            sql.push_str(" AND a.hidden=0 ");
        }
        if let Some((y, m)) = q.month {
            let (start, end) = month_bounds(y, m);
            args.push(Box::new(start));
            args.push(Box::new(end));
            sql.push_str(&format!(
                " AND COALESCE(t.user_posted_at, t.posted_at) >= ?{} AND COALESCE(t.user_posted_at, t.posted_at) < ?{} ",
                args.len() - 1,
                args.len()
            ));
        }
        let needle = q.search.trim().to_lowercase();
        if !needle.is_empty() {
            args.push(Box::new(needle.clone()));
            let n = args.len();
            let mut clause = format!(
                " AND (instr(lower(COALESCE(t.user_payee, t.description)), ?{n}) > 0
                       OR instr(lower(COALESCE(t.notes, '')), ?{n}) > 0"
            );
            if let Ok(cents) = crate::money::parse_cents(&needle) {
                args.push(Box::new(cents.abs()));
                clause.push_str(&format!(
                    " OR ABS(COALESCE(t.user_amount_cents, t.amount_cents)) = ?{}",
                    args.len()
                ));
            }
            clause.push_str(") ");
            sql.push_str(&clause);
        }
        sql.push_str(" ORDER BY COALESCE(t.user_posted_at, t.posted_at) DESC, t.id");
        let mut stmt = self.conn().prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            let split_count: i64 = r.get(12)?;
            Ok(Txn {
                id: r.get(0)?,
                account_id: r.get(1)?,
                account_name: r.get(2)?,
                posted_at: r.get(3)?,
                amount_cents: r.get(4)?,
                payee: r.get(5)?,
                notes: r.get(6)?,
                category_id: r.get(7)?,
                category_name: r.get(8)?,
                excluded: r.get::<_, i64>(9)? != 0,
                pending: r.get::<_, i64>(10)? != 0,
                is_transfer: r.get::<_, i64>(11)? != 0,
                has_splits: split_count > 0,
                parent_id: r.get(13)?,
                is_income: r.get::<_, i64>(14)? != 0,
                payment: r
                    .get::<_, Option<String>>(15)?
                    .as_deref()
                    .and_then(PaymentKind::parse),
                categorized_by: r.get(16)?,
                ai_failed_at: r.get(17)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Distinct (year, month) pairs that have transactions, newest first.
    pub fn list_months(&self) -> Result<Vec<(i32, u32)>, Error> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT
                CAST(strftime('%Y', COALESCE(user_posted_at, posted_at), 'unixepoch') AS INTEGER),
                CAST(strftime('%m', COALESCE(user_posted_at, posted_at), 'unixepoch') AS INTEGER)
             FROM transactions t JOIN accounts a ON a.id = t.account_id
             WHERE t.deleted=0 AND t.parent_id IS NULL AND a.hidden=0
             ORDER BY 1 DESC, 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, u32>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// How many rows still need a category (the Activity default list).
    pub fn uncategorized_count(&self) -> Result<u32, Error> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM transactions t JOIN accounts a ON a.id = t.account_id
             WHERE t.deleted=0 AND t.parent_id IS NULL AND t.category_id IS NULL AND t.excluded=0
               AND t.is_income=0 AND a.hidden=0
               AND t.transfer_peer_id IS NULL AND t.transfer_account_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Apply a patch to one or more transactions. Setting a category learns the payee and
    /// auto-categorizes similar uncategorized rows. Returns how many extra rows were filled.
    pub fn patch_transactions(&self, ids: &[String], patch: &TxnPatch) -> Result<u32, Error> {
        let mut learned = false;
        for id in ids {
            if let Some(cat) = &patch.category_id {
                self.conn().execute(
                    "UPDATE transactions SET category_id=?2, categorized_by='user' WHERE id=?1",
                    params![id, cat],
                )?;
                if let Some(cid) = cat {
                    let payee: String = self.conn().query_row(
                        "SELECT COALESCE(user_payee, description) FROM transactions WHERE id=?1",
                        params![id],
                        |r| r.get(0),
                    )?;
                    let key = normalize_payee(&payee);
                    if !key.is_empty() {
                        self.conn().execute(
                            "INSERT INTO payee_memory (normalized_payee, category_id) VALUES (?1,?2)
                             ON CONFLICT(normalized_payee) DO UPDATE SET category_id=excluded.category_id",
                            params![key, cid],
                        )?;
                    }
                    learned = true;
                }
            }
            if let Some(payee) = &patch.payee {
                self.conn().execute(
                    "UPDATE transactions SET user_payee=?2 WHERE id=?1",
                    params![id, payee],
                )?;
            }
            if let Some(notes) = &patch.notes {
                self.conn().execute(
                    "UPDATE transactions SET notes=?2 WHERE id=?1",
                    params![id, notes],
                )?;
            }
            if let Some(ex) = patch.excluded {
                self.conn().execute(
                    "UPDATE transactions SET excluded=?2 WHERE id=?1",
                    params![id, ex as i64],
                )?;
            }
            if let Some(amt) = patch.amount_cents {
                self.conn().execute(
                    "UPDATE transactions SET user_amount_cents=?2 WHERE id=?1",
                    params![id, amt],
                )?;
            }
            if let Some(posted) = patch.posted_at {
                self.conn().execute(
                    "UPDATE transactions SET user_posted_at=?2 WHERE id=?1",
                    params![id, posted],
                )?;
            }
            if let Some(xfer) = &patch.transfer_account_id {
                // Un-marking a transfer also forgets which kind of payment it was.
                self.conn().execute(
                    "UPDATE transactions SET transfer_account_id=?2,
                        transfer_kind=CASE WHEN ?2 IS NULL THEN NULL ELSE transfer_kind END
                     WHERE id=?1",
                    params![id, xfer],
                )?;
            }
            if let Some(kind) = patch.payment {
                self.conn().execute(
                    "UPDATE transactions SET transfer_kind=?2,
                        transfer_account_id=CASE WHEN ?2 IS NULL THEN transfer_account_id
                                                 ELSE COALESCE(transfer_account_id, 'manual') END
                     WHERE id=?1",
                    params![id, kind.map(PaymentKind::as_str)],
                )?;
            }
            if let Some(income) = patch.income {
                self.conn().execute(
                    "UPDATE transactions SET is_income=?2 WHERE id=?1",
                    params![id, income as i64],
                )?;
            }
        }
        let extra = if learned {
            self.apply_auto_categorize()?
        } else {
            0
        };
        self.persist()?;
        Ok(extra)
    }

    pub fn split_transaction(
        &self,
        parent_id: &str,
        parts: &[(Option<String>, i64)],
    ) -> Result<(), Error> {
        let (account_id, amount, posted, desc): (String, i64, i64, String) = self.conn().query_row(
            "SELECT account_id, COALESCE(user_amount_cents, amount_cents), COALESCE(user_posted_at, posted_at), COALESCE(user_payee, description)
             FROM transactions WHERE id=?1",
            params![parent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let sum: i64 = parts.iter().map(|p| p.1).sum();
        if sum != amount {
            return Err(Error::user(format!(
                "Splits must sum to {} cents (got {sum}).",
                amount
            )));
        }
        self.conn().execute(
            "DELETE FROM transactions WHERE parent_id=?1",
            params![parent_id],
        )?;
        for (i, (cat, cents)) in parts.iter().enumerate() {
            let id = Uuid::new_v4().to_string();
            let remote = format!("{parent_id}:split:{i}");
            self.conn().execute(
                "INSERT INTO transactions (id, account_id, remote_id, posted_at, amount_cents, description, parent_id, category_id, categorized_by)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'user')",
                params![id, account_id, remote, posted, cents, desc, parent_id, cat],
            )?;
        }
        self.persist()?;
        Ok(())
    }

    /// Every category's cap and spend this month, off-budget rows included (with no cap) so
    /// the Categories table can list them. Month filters on `in_budget`. A budgeted child's
    /// spend is added into its parent's row; an off-budget child's never is.
    pub fn month_budget(&self, year: i32, month: u32) -> Result<Vec<MonthRow>, Error> {
        let cats = self.list_categories()?;
        let mut out: Vec<MonthRow> = Vec::new();
        for cat in cats {
            let in_budget = cat.is_budgeted();
            let cap = if in_budget {
                self.cap_for(&cat.id, year, month)?
            } else {
                0
            };
            let spent = self.category_spent(&cat.id, year, month)?;
            if in_budget {
                if let Some(pid) = &cat.parent_id {
                    if let Some(parent) = out.iter_mut().find(|r| &r.category_id == pid) {
                        parent.spent_cents += spent;
                    }
                }
            }
            out.push(MonthRow {
                category_id: cat.id,
                category_name: cat.name,
                parent_id: cat.parent_id,
                in_budget,
                cap_cents: cap,
                spent_cents: spent,
            });
        }
        Ok(out)
    }

    /// The cap in force for a category in a month: the latest one set at or before it.
    /// Months before the first cap was set have none.
    fn cap_for(&self, category_id: &str, year: i32, month: u32) -> Result<i64, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT cap_cents FROM budgets
                 WHERE category_id=?1 AND year*12+month <= ?2
                 ORDER BY year DESC, month DESC LIMIT 1",
                params![category_id, year as i64 * 12 + month as i64],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Outflows that count toward a category cap this calendar month.
    pub fn category_spent(&self, category_id: &str, year: i32, month: u32) -> Result<i64, Error> {
        let (start, end) = month_bounds(year, month);
        let mut stmt = self.conn().prepare(
            "SELECT COALESCE(user_amount_cents, amount_cents), id
             FROM transactions
             WHERE deleted=0 AND excluded=0
               AND transfer_peer_id IS NULL AND transfer_account_id IS NULL
               AND account_id NOT IN (SELECT id FROM accounts WHERE hidden=1)
               AND category_id = ?1
               AND parent_id IS NULL
               AND COALESCE(user_posted_at, posted_at) >= ?2
               AND COALESCE(user_posted_at, posted_at) < ?3",
        )?;
        let mut rows = stmt.query(params![category_id, start, end])?;
        let mut total = 0i64;
        while let Some(row) = rows.next()? {
            let amount: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let child_sum: Option<i64> = self
                .conn()
                .query_row(
                    "SELECT SUM(COALESCE(user_amount_cents, amount_cents)) FROM transactions
                     WHERE parent_id=?1 AND deleted=0 AND excluded=0 AND category_id=?2
                       AND transfer_peer_id IS NULL AND transfer_account_id IS NULL",
                    params![id, category_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(sum) = child_sum {
                if sum != 0 || self.has_children(&id)? {
                    // Parent with splits: spend is on children, not parent.
                    continue;
                }
            }
            if amount < 0 {
                total += -amount;
            }
        }
        // Split children whose parent is not in this category still count here.
        let mut stmt = self.conn().prepare(
            "SELECT COALESCE(s.user_amount_cents, s.amount_cents)
             FROM transactions s
             JOIN transactions p ON p.id = s.parent_id
             WHERE s.deleted=0 AND s.excluded=0 AND s.category_id=?1
               AND s.transfer_peer_id IS NULL AND s.transfer_account_id IS NULL
               AND p.deleted=0 AND p.excluded=0
               AND p.account_id NOT IN (SELECT id FROM accounts WHERE hidden=1)
               AND COALESCE(p.user_posted_at, p.posted_at) >= ?2
               AND COALESCE(p.user_posted_at, p.posted_at) < ?3",
        )?;
        let mut rows = stmt.query(params![category_id, start, end])?;
        while let Some(row) = rows.next()? {
            let amount: i64 = row.get(0)?;
            if amount < 0 {
                total += -amount;
            }
        }
        Ok(total)
    }

    /// Inflows marked as income this calendar month (visible accounts, not excluded).
    pub fn month_income(&self, year: i32, month: u32) -> Result<i64, Error> {
        let (start, end) = month_bounds(year, month);
        let total: Option<i64> = self.conn().query_row(
            "SELECT SUM(COALESCE(t.user_amount_cents, t.amount_cents))
             FROM transactions t JOIN accounts a ON a.id = t.account_id
             WHERE t.deleted=0 AND t.excluded=0 AND t.is_income=1 AND t.parent_id IS NULL
               AND a.hidden=0
               AND COALESCE(t.user_amount_cents, t.amount_cents) > 0
               AND COALESCE(t.user_posted_at, t.posted_at) >= ?1
               AND COALESCE(t.user_posted_at, t.posted_at) < ?2",
            params![start, end],
            |r| r.get(0),
        )?;
        Ok(total.unwrap_or(0))
    }

    fn has_children(&self, id: &str) -> Result<bool, Error> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM transactions WHERE parent_id=?1 AND deleted=0",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Apply rules then payee memory to non-user rows. Rebuilds memory keys from user categorizations.
    pub fn auto_categorize_new(&self) -> Result<u32, Error> {
        self.rebuild_payee_memory()?;
        let n = self.apply_auto_categorize()?;
        self.persist()?;
        Ok(n)
    }

    /// Rows the AI pass may categorize: uncategorized after rules and memory ran, not flagged,
    /// not on a hidden account. `retry_failed` includes rows whose last AI attempt errored;
    /// the automatic after-sync pass leaves them out. Returns (id, payee, amount_cents).
    pub fn list_ai_candidates(
        &self,
        retry_failed: bool,
    ) -> Result<Vec<(String, String, i64)>, Error> {
        let mut sql = String::from(
            "SELECT t.id, COALESCE(t.user_payee, t.description), COALESCE(t.user_amount_cents, t.amount_cents)
             FROM transactions t JOIN accounts a ON a.id = t.account_id
             WHERE t.deleted=0 AND t.parent_id IS NULL AND t.category_id IS NULL AND t.excluded=0
               AND t.is_income=0 AND a.hidden=0
               AND t.transfer_peer_id IS NULL AND t.transfer_account_id IS NULL ",
        );
        if !retry_failed {
            sql.push_str(" AND t.ai_failed_at IS NULL ");
        }
        sql.push_str(" ORDER BY COALESCE(t.user_posted_at, t.posted_at) DESC, t.id");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Flag rows whose AI call just errored so automatic passes stop retrying them.
    pub fn mark_ai_failed(&self, ids: &[String]) -> Result<(), Error> {
        for id in ids {
            self.conn().execute(
                "UPDATE transactions SET ai_failed_at=unixepoch() WHERE id=?1 AND deleted=0",
                params![id],
            )?;
        }
        Ok(())
    }

    /// Drop the failure flag once a row's payee has a real answer again.
    pub fn clear_ai_failed(&self, ids: &[String]) -> Result<(), Error> {
        for id in ids {
            self.conn().execute(
                "UPDATE transactions SET ai_failed_at=NULL WHERE id=?1 AND ai_failed_at IS NOT NULL",
                params![id],
            )?;
        }
        Ok(())
    }

    /// What the AI pass would send for one transaction, whatever its state: (payee, amount).
    /// `None` when the row is gone. Used by the debug trace.
    pub fn ai_input_for(&self, txn_id: &str) -> Result<Option<(String, i64)>, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT COALESCE(user_payee, description), COALESCE(user_amount_cents, amount_cents)
                 FROM transactions WHERE id=?1 AND deleted=0",
                params![txn_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Cached AI answer for a normalized payee: (category_id or None for "other", confidence).
    /// A recorded failure is not an answer, and neither is one saved before exchanges were
    /// kept (`exchanges IS NULL`): both make the payee get asked again, so the trace fills in.
    pub fn ai_answer(
        &self,
        normalized_payee: &str,
    ) -> Result<Option<(Option<String>, f64)>, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT category_id, confidence FROM ai_answers
                 WHERE normalized_payee=?1 AND error IS NULL AND exchanges IS NOT NULL",
                params![normalized_payee],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Remember an AI answer so the payee is not asked again until categories change.
    /// `exchanges` is the recorded request/response trail, kept for the debug view.
    pub fn remember_ai_answer(
        &self,
        normalized_payee: &str,
        category_id: Option<&str>,
        confidence: f64,
        provider: &str,
        exchanges: &[AiExchange],
    ) -> Result<(), Error> {
        let exchanges_json = serde_json::to_string(exchanges)
            .map_err(|_| Error::user("Could not record the AI exchange."))?;
        self.conn().execute(
            "INSERT INTO ai_answers (normalized_payee, category_id, confidence, provider, asked_at, exchanges, error)
             VALUES (?1, ?2, ?3, ?4, unixepoch(), ?5, NULL)
             ON CONFLICT(normalized_payee) DO UPDATE SET category_id=excluded.category_id,
                confidence=excluded.confidence, provider=excluded.provider, asked_at=excluded.asked_at,
                exchanges=excluded.exchanges, error=NULL",
            params![normalized_payee, category_id, confidence, provider, exchanges_json],
        )?;
        Ok(())
    }

    /// Remember that asking about a payee failed, with the sanitized error and the round trips
    /// that led to it, so the trace can show what went wrong. Replaces any earlier answer; the
    /// payee counts as unanswered until a later call succeeds.
    pub fn remember_ai_failure(
        &self,
        normalized_payee: &str,
        provider: &str,
        error: &str,
        exchanges: &[AiExchange],
    ) -> Result<(), Error> {
        let exchanges_json = serde_json::to_string(exchanges)
            .map_err(|_| Error::user("Could not record the AI exchange."))?;
        self.conn().execute(
            "INSERT INTO ai_answers (normalized_payee, category_id, confidence, provider, asked_at, exchanges, error)
             VALUES (?1, NULL, 0, ?2, unixepoch(), ?3, ?4)
             ON CONFLICT(normalized_payee) DO UPDATE SET category_id=NULL, confidence=0,
                provider=excluded.provider, asked_at=excluded.asked_at,
                exchanges=excluded.exchanges, error=excluded.error",
            params![normalized_payee, provider, exchanges_json, error],
        )?;
        Ok(())
    }

    /// The recorded AI answer for one transaction's payee, if it was ever asked. Looks the
    /// payee up the same way the AI pass does, so it matches whatever answer applied.
    pub fn ai_record_for(&self, txn_id: &str) -> Result<Option<AiRecord>, Error> {
        let Some((payee, _)) = self.ai_input_for(txn_id)? else {
            return Ok(None);
        };
        let key = normalize_payee(&payee);
        let row = self
            .conn()
            .query_row(
                "SELECT category_id, confidence, provider, asked_at, exchanges, error
                 FROM ai_answers WHERE normalized_payee=?1",
                params![key],
                |r| {
                    Ok(AiRecord {
                        payee: payee.clone(),
                        category_id: r.get(0)?,
                        category_name: None,
                        confidence: r.get(1)?,
                        provider: r.get(2)?,
                        asked_at: r.get(3)?,
                        exchanges: Vec::new(),
                        error: r.get(5)?,
                    })
                    .map(|rec| (rec, r.get::<_, Option<String>>(4)))
                },
            )
            .optional()?;
        let Some((mut rec, exchanges)) = row else {
            return Ok(None);
        };
        let exchanges = exchanges?;
        rec.category_name = match &rec.category_id {
            Some(id) => self
                .conn()
                .query_row(
                    &format!(
                        "SELECT {CATEGORY_LABEL_SQL} FROM categories c
                         LEFT JOIN categories pc ON pc.id = c.parent_id WHERE c.id=?1"
                    ),
                    params![id],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        rec.exchanges = exchanges
            .as_deref()
            .map(serde_json::from_str::<Vec<AiExchange>>)
            .transpose()
            .map_err(|_| Error::user("The recorded AI exchange is unreadable."))?
            .unwrap_or_default();
        Ok(Some(rec))
    }

    /// Set a category on rows the AI picked for. Only touches rows that are still uncategorized,
    /// so a rule or hand edit that landed meanwhile wins. Returns how many rows changed.
    pub fn apply_ai_category(&self, ids: &[String], category_id: &str) -> Result<u32, Error> {
        let mut n = 0u32;
        for id in ids {
            n += self.conn().execute(
                "UPDATE transactions SET category_id=?2, categorized_by='ai'
                 WHERE id=?1 AND category_id IS NULL AND deleted=0",
                params![id, category_id],
            )? as u32;
        }
        Ok(n)
    }

    /// Replace payee_memory using current user categorizations and the latest merchant key.
    fn rebuild_payee_memory(&self) -> Result<(), Error> {
        self.conn().execute("DELETE FROM payee_memory", [])?;
        let mut stmt = self.conn().prepare(
            "SELECT COALESCE(user_payee, description), category_id
             FROM transactions
             WHERE deleted=0 AND parent_id IS NULL AND categorized_by='user' AND category_id IS NOT NULL
             ORDER BY COALESCE(user_posted_at, posted_at) ASC, id ASC",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for (payee, cat) in rows {
            let key = normalize_payee(&payee);
            if key.is_empty() {
                continue;
            }
            self.conn().execute(
                "INSERT INTO payee_memory (normalized_payee, category_id) VALUES (?1,?2)
                 ON CONFLICT(normalized_payee) DO UPDATE SET category_id=excluded.category_id",
                params![key, cat],
            )?;
        }
        Ok(())
    }

    /// Apply rules, then payee memory, to rows the user has not categorized by hand. Returns
    /// how many rows changed: newly categorized, or newly flagged by a transfer/income/exclude rule.
    fn apply_auto_categorize(&self) -> Result<u32, Error> {
        let rules = self.list_rules()?;
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, COALESCE(user_payee, description), COALESCE(user_amount_cents, amount_cents), category_id
             FROM transactions
             WHERE deleted=0 AND parent_id IS NULL AND (categorized_by IS NULL OR categorized_by != 'user')
               AND (category_id IS NULL OR categorized_by IN ('rule','memory','ai'))",
        )?;
        let rows: Vec<(String, String, String, i64, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut n = 0u32;
        for (id, account_id, payee, amount, prev) in rows {
            if let Some(rule) = match_rules(&rules, &payee, amount, &account_id) {
                if self.apply_rule_action(rule, &id, prev.as_deref())? {
                    n += 1;
                }
                continue;
            }
            let key = normalize_payee(&payee);
            if key.is_empty() {
                continue;
            }
            let mem: Option<String> = self
                .conn()
                .query_row(
                    "SELECT category_id FROM payee_memory WHERE normalized_payee=?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(cat) = mem {
                self.conn().execute(
                    "UPDATE transactions SET category_id=?2, categorized_by='memory' WHERE id=?1",
                    params![id, cat],
                )?;
                if prev.is_none() {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Run one rule's action on one row. Flag rules only ever set the flag; the row keeps
    /// whatever category it had. Returns true when the row actually changed.
    fn apply_rule_action(
        &self,
        rule: &Rule,
        id: &str,
        prev_category: Option<&str>,
    ) -> Result<bool, Error> {
        Ok(match rule.action {
            RuleAction::Category => {
                self.conn().execute(
                    "UPDATE transactions SET category_id=?2, categorized_by='rule' WHERE id=?1",
                    params![id, rule.category_id],
                )?;
                prev_category != rule.category_id.as_deref()
            }
            RuleAction::Transfer => {
                self.conn().execute(
                    "UPDATE transactions SET transfer_account_id='rule'
                 WHERE id=?1 AND transfer_account_id IS NULL AND transfer_peer_id IS NULL",
                    params![id],
                )? > 0
            }
            RuleAction::CardPayment | RuleAction::LoanPayment => {
                let kind = if rule.action == RuleAction::CardPayment {
                    PaymentKind::Card
                } else {
                    PaymentKind::Loan
                };
                self.conn().execute(
                    "UPDATE transactions SET transfer_kind=?2,
                        transfer_account_id=COALESCE(transfer_account_id, 'rule')
                     WHERE id=?1 AND (transfer_kind IS NULL OR transfer_kind != ?2)",
                    params![id, kind.as_str()],
                )? > 0
            }
            RuleAction::Income => {
                self.conn().execute(
                    "UPDATE transactions SET is_income=1 WHERE id=?1 AND is_income=0",
                    params![id],
                )? > 0
            }
            RuleAction::Exclude => {
                self.conn().execute(
                    "UPDATE transactions SET excluded=1 WHERE id=?1 AND excluded=0",
                    params![id],
                )? > 0
            }
        })
    }
}

/// Case-insensitive wildcard match over the whole of `text`: `*` matches any run of characters
/// (including none) and `?` matches exactly one. A pattern with no wildcards only matches an
/// identical description, so "contains costco" is written `*costco*`.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().map(straight_quote).collect();
    let t: Vec<char> = text.to_lowercase().chars().map(straight_quote).collect();
    let (mut pi, mut ti) = (0, 0);
    // Where the last `*` was and how much text it has swallowed so far, for backtracking.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi + 1, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// A typographic quote as its plain ASCII form, so the curly `’` a webview's smart-quote
/// substitution slips into a typed pattern still matches the bank's `'`.
fn straight_quote(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        other => other,
    }
}

/// `s` with every typographic quote replaced by its plain ASCII form. See [`straight_quote`].
pub fn straighten_quotes(s: &str) -> String {
    s.chars().map(straight_quote).collect()
}

/// The stored pattern and category for a rule, or why it cannot be saved: a categorize rule
/// needs a category, and a pattern must have at least one alternative. The pattern comes back
/// in canonical `a | b` form so the list and the matcher agree on the alternatives.
fn validate_rule(rule: &Rule) -> Result<(Option<String>, Option<String>), Error> {
    let category_id = match rule.action {
        RuleAction::Category => match &rule.category_id {
            Some(c) if !c.is_empty() => Some(c.clone()),
            _ => return Err(Error::user("A categorize rule needs a category.")),
        },
        _ => None,
    };
    let description_pattern = match &rule.description_pattern {
        Some(p) => {
            let joined = straighten_quotes(&split_patterns(p).collect::<Vec<_>>().join(" | "));
            if joined.is_empty() {
                return Err(Error::user("A rule needs a pattern."));
            }
            Some(joined)
        }
        None => None,
    };
    Ok((description_pattern, category_id))
}

/// The non-empty, trimmed alternatives of a rule pattern: `arco shop* | beta place c` is two.
pub fn split_patterns(pattern: &str) -> impl Iterator<Item = &str> {
    pattern.split('|').map(str::trim).filter(|p| !p.is_empty())
}

/// Whether any `|`-separated alternative in `pattern` [`wildcard_match`]es `text`. A pattern
/// with no alternatives (empty, or only `|` and spaces) matches nothing.
pub fn pattern_match(pattern: &str, text: &str) -> bool {
    split_patterns(pattern).any(|p| wildcard_match(p, text))
}

/// First enabled rule whose conditions all hold. A rule with no conditions never matches.
pub fn match_rules<'a>(
    rules: &'a [Rule],
    payee: &str,
    amount: i64,
    account_id: &str,
) -> Option<&'a Rule> {
    let payee_l = payee.to_lowercase();
    for rule in rules.iter().filter(|r| r.enabled) {
        if let Some(acc) = &rule.account_id {
            if acc != account_id {
                continue;
            }
        }
        if let Some(min) = rule.amount_min_cents {
            if amount < min {
                continue;
            }
        }
        if let Some(max) = rule.amount_max_cents {
            if amount > max {
                continue;
            }
        }
        if let Some(pattern) = &rule.description_pattern {
            if !pattern_match(pattern, &payee_l) {
                continue;
            }
        }
        if let Some(re) = &rule.description_regex {
            if let Ok(rx) = Regex::new(re) {
                if !rx.is_match(payee) {
                    continue;
                }
            } else {
                continue;
            }
        }
        if rule.description_pattern.is_none()
            && rule.description_regex.is_none()
            && rule.amount_min_cents.is_none()
            && rule.amount_max_cents.is_none()
            && rule.account_id.is_none()
        {
            continue;
        }
        return Some(rule);
    }
    None
}

pub fn month_bounds(year: i32, month: u32) -> (i64, i64) {
    let start = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap();
    let end = if month == 12 {
        Utc.with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0).unwrap()
    } else {
        Utc.with_ymd_and_hms(year, month + 1, 1, 0, 0, 0).unwrap()
    };
    (start.timestamp(), end.timestamp())
}

pub fn current_year_month() -> (i32, u32) {
    let n = Utc::now();
    (n.year(), n.month())
}

/// Trim and reject blank category names.
fn require_category_name(name: &str) -> Result<&str, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::user("Category name can't be empty."));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_label_adds_bank_unless_redundant() {
        let acct = |name: &str, bank: Option<&str>| Account {
            id: "a".into(),
            connection_id: "c".into(),
            name: name.into(),
            institution: bank.map(String::from),
            hidden: false,
        };
        assert_eq!(
            acct("Checking", Some("Example Bank")).label(),
            "Checking · Example Bank"
        );
        assert_eq!(acct("Checking", None).label(), "Checking");
        assert_eq!(
            acct("Example Bank Checking", Some("Example Bank")).label(),
            "Example Bank Checking"
        );
    }
    use crate::providers::{AccountSet, ConnectionSecrets, NormalizedAccount, NormalizedTxn};
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let s = Store::open(dir.path(), "pass").unwrap();
        (dir, s)
    }

    fn seed_txn(s: &Store, amount: i64, desc: &str, posted: i64) -> String {
        let secrets = ConnectionSecrets {
            inner: "https://u:p@example.com/simplefin".into(),
        };
        let cid = s.add_connection("simplefin", "Demo", &secrets).unwrap();
        let set = AccountSet {
            errors: vec![],
            accounts: vec![NormalizedAccount {
                remote_id: "acct".into(),
                conn_id: "c".into(),
                name: "Checking".into(),
                institution: None,
                currency: "USD".into(),
                balance_cents: 0,
                available_cents: None,
                balance_date: posted,
                transactions: vec![NormalizedTxn {
                    remote_id: Uuid::new_v4().to_string(),
                    posted,
                    transacted_at: None,
                    amount_cents: amount,
                    description: desc.into(),
                    pending: false,
                    raw: None,
                }],
                raw: None,
            }],
        };
        s.upsert_imported(&cid, &set).unwrap();
        s.list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.payee == desc && t.posted_at == posted)
            .expect("seeded txn")
            .id
    }

    fn categorized_by(s: &Store, id: &str) -> Option<String> {
        s.conn()
            .query_row(
                "SELECT categorized_by FROM transactions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn exclude_and_transfer_skip_spend() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let cat = s.add_category("Food").unwrap();
        s.set_budget(&cat, y, m, 10000).unwrap();
        let id = seed_txn(&s, -2500, "Cafe", start + 10);
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 2500);
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                excluded: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 0);
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                excluded: Some(false),
                transfer_account_id: Some(Some("other".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 0);
    }

    #[test]
    fn splits_count_children_not_parent() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let food = s.add_category("Food").unwrap();
        let gas = s.add_category("Gas").unwrap();
        let id = seed_txn(&s, -3000, "Costco", start + 10);
        s.split_transaction(
            &id,
            &[(Some(food.clone()), -2000), (Some(gas.clone()), -1000)],
        )
        .unwrap();
        assert_eq!(s.category_spent(&food, y, m).unwrap(), 2000);
        assert_eq!(s.category_spent(&gas, y, m).unwrap(), 1000);
    }

    #[test]
    fn wildcard_match_semantics() {
        // Whole-string match, case-insensitive.
        assert!(wildcard_match("costco", "COSTCO"));
        assert!(!wildcard_match("costco", "COSTCO WHSE #123"));
        // `*` is any run, including none.
        assert!(wildcard_match("*costco*", "COSTCO WHSE #123"));
        assert!(wildcard_match("*costco*", "Costco"));
        assert!(wildcard_match("costco*", "COSTCO WHSE #123"));
        assert!(!wildcard_match("costco*", "WHSE COSTCO"));
        assert!(wildcard_match("*payroll", "ACME CORP PAYROLL"));
        assert!(wildcard_match("acme*payroll", "ACME CORP PAYROLL"));
        assert!(!wildcard_match("acme*payroll", "ACME CORP PAYROLL 2"));
        // `?` is exactly one character.
        assert!(wildcard_match("uber ?rip", "Uber Trip"));
        assert!(!wildcard_match("uber ?rip", "Uber  Trip"));
        // Backtracking picks the later `*` split when the first attempt fails.
        assert!(wildcard_match("*a*b", "xaxaxb"));
        assert!(!wildcard_match("*a*b", "xaxaxbx"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
    }

    #[test]
    fn pattern_match_takes_any_alternative() {
        assert!(pattern_match("arco shop* | beta place c", "ARCO SHOP #12"));
        assert!(pattern_match("arco shop* | beta place c", "Beta Place C"));
        assert!(!pattern_match("arco shop* | beta place c", "Beta Place D"));
        // Spaces around `|` are ignored; empty alternatives are skipped rather than matching everything.
        assert!(pattern_match("arco*|beta*", "beta"));
        assert!(!pattern_match("arco* | ", "anything"));
        assert!(!pattern_match("|", ""));
        assert!(!pattern_match("", "x"));
        assert_eq!(
            split_patterns(" a | | b ").collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn curly_quotes_match_and_are_stored_straight() {
        assert_eq!(
            straighten_quotes("Trader Joe’s “Shop”"),
            "Trader Joe's \"Shop\""
        );
        assert!(wildcard_match("*joe’s*", "TRADER JOE'S #123"));
        assert!(wildcard_match("*joe's*", "TRADER JOE’S #123"));
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        let (start, _) = month_bounds(2026, 9);
        let joes = seed_txn(&s, -900, "TRADER JOE'S #123", start + 1);
        let (id, n) = s
            .create_rule(&Rule::pattern(
                "*joe’s*",
                RuleAction::Category,
                Some(food.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(n, 1);
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("*joe's*"));
        let txns = s.list_transactions(false, None).unwrap();
        assert_eq!(
            txns.iter()
                .find(|t| t.id == joes)
                .unwrap()
                .category_id
                .as_deref(),
            Some(food.as_str())
        );
        // Adding the curly spelling of an alternative the rule already has is a no-op.
        s.add_rule_pattern(&id, "*JOE’S*").unwrap();
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("*joe's*"));
        s.update_rule(&id, "mcdonald’s", RuleAction::Category, Some(food.clone()))
            .unwrap();
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("mcdonald's"));
    }

    #[test]
    fn rule_with_alternatives_matches_each_and_stores_canonical_form() {
        let (_d, s) = store();
        let cat = s.add_category("Shopping").unwrap();
        let (start, _) = month_bounds(2026, 9);
        let arco = seed_txn(&s, -900, "ARCO SHOP #12", start + 1);
        let beta = seed_txn(&s, -900, "Beta Place C", start + 2);
        let other = seed_txn(&s, -900, "Gamma Store", start + 3);
        let (id, n) = s
            .create_rule(&Rule::pattern(
                "arco shop*|  beta place c ",
                RuleAction::Category,
                Some(cat.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(n, 2);
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(
            rule.description_pattern.as_deref(),
            Some("arco shop* | beta place c")
        );
        let txns = s.list_transactions(false, None).unwrap();
        let by_id = |id: &str| {
            txns.iter()
                .find(|t| t.id == id)
                .unwrap()
                .category_id
                .clone()
        };
        assert_eq!(by_id(&arco).as_deref(), Some(cat.as_str()));
        assert_eq!(by_id(&beta).as_deref(), Some(cat.as_str()));
        assert_eq!(by_id(&other), None);
        // New rows matching either alternative are picked up on sync too.
        seed_txn(&s, -500, "ARCO SHOP #99", start + 4);
        assert_eq!(s.auto_categorize_new().unwrap(), 1);
    }

    #[test]
    fn add_rule_pattern_appends_alternatives_and_reapplies() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        let (start, _) = month_bounds(2026, 9);
        let arco = seed_txn(&s, -900, "ARCO SHOP #12", start + 1);
        let beta = seed_txn(&s, -900, "Beta Place C", start + 2);
        let (id, n) = s
            .create_rule(&Rule::pattern(
                "arco*",
                RuleAction::Category,
                Some(food.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(n, 1);
        // The new alternative is appended; only the newly matched row changes.
        let n = s.add_rule_pattern(&id, "*beta*").unwrap();
        assert_eq!(n, 1);
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("arco* | *beta*"));
        assert_eq!(rule.action, RuleAction::Category);
        assert_eq!(rule.category_id.as_deref(), Some(food.as_str()));
        let txns = s.list_transactions(false, None).unwrap();
        let by_id = |id: &str| txns.iter().find(|t| t.id == id).unwrap().clone();
        assert_eq!(by_id(&arco).category_id.as_deref(), Some(food.as_str()));
        assert_eq!(by_id(&beta).category_id.as_deref(), Some(food.as_str()));
        // Duplicates (case-insensitive) are not repeated; blanks are rejected; unknown rules error.
        s.add_rule_pattern(&id, "ARCO* | gamma").unwrap();
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(
            rule.description_pattern.as_deref(),
            Some("arco* | *beta* | gamma")
        );
        assert!(s.add_rule_pattern(&id, " | ").is_err());
        assert!(s.add_rule_pattern("nope", "x").is_err());
    }

    #[test]
    fn update_rule_changes_pattern_and_action_and_reapplies() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        let shop = s.add_category("Shopping").unwrap();
        let (start, _) = month_bounds(2026, 9);
        let arco = seed_txn(&s, -900, "ARCO SHOP #12", start + 1);
        let beta = seed_txn(&s, -900, "Beta Place C", start + 2);
        let (id, n) = s
            .create_rule(&Rule::pattern(
                "arco*",
                RuleAction::Category,
                Some(food.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(n, 1);
        // Widen the pattern and switch the category: both rows now get Shopping.
        let n = s
            .update_rule(
                &id,
                "arco* | beta*",
                RuleAction::Category,
                Some(shop.clone()),
            )
            .unwrap();
        assert_eq!(n, 2);
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("arco* | beta*"));
        assert_eq!(rule.category_id.as_deref(), Some(shop.as_str()));
        assert_eq!(rule.priority, 1);
        let txns = s.list_transactions(false, None).unwrap();
        let by_id = |id: &str| txns.iter().find(|t| t.id == id).unwrap().clone();
        assert_eq!(by_id(&arco).category_id.as_deref(), Some(shop.as_str()));
        assert_eq!(by_id(&beta).category_id.as_deref(), Some(shop.as_str()));
        // Switching to a flag action drops the category from the rule but not from the rows.
        let n = s
            .update_rule(&id, "beta*", RuleAction::Transfer, None)
            .unwrap();
        assert_eq!(n, 1);
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.action, RuleAction::Transfer);
        assert_eq!(rule.category_id, None);
        let txns = s.list_transactions(false, None).unwrap();
        let by_id = |id: &str| txns.iter().find(|t| t.id == id).unwrap().clone();
        assert!(by_id(&beta).is_transfer);
        assert_eq!(by_id(&arco).category_id.as_deref(), Some(shop.as_str()));
    }

    #[test]
    fn update_rule_validates_and_needs_an_existing_rule() {
        let (_d, s) = store();
        let cat = s.add_category("Food").unwrap();
        let id = s
            .add_rule(&Rule::pattern(
                "a",
                RuleAction::Category,
                Some(cat.clone()),
                1,
            ))
            .unwrap();
        assert!(s
            .update_rule(&id, " | ", RuleAction::Category, Some(cat.clone()))
            .is_err());
        assert!(s.update_rule(&id, "a", RuleAction::Category, None).is_err());
        assert!(s
            .update_rule("nope", "a", RuleAction::Category, Some(cat.clone()))
            .is_err());
        // A failed update leaves the rule untouched.
        let rule = s
            .list_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(rule.description_pattern.as_deref(), Some("a"));
        assert_eq!(rule.action, RuleAction::Category);
    }

    #[test]
    fn rule_pattern_of_only_separators_is_rejected() {
        let (_d, s) = store();
        let cat = s.add_category("Shopping").unwrap();
        let err = s
            .add_rule(&Rule::pattern(" | ", RuleAction::Category, Some(cat), 1))
            .unwrap_err();
        assert!(err.as_user_message().contains("pattern"), "{err:?}");
        assert!(s.list_rules().unwrap().is_empty());
    }

    #[test]
    fn preview_and_suggestion_cover_alternatives() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let arco = seed_txn(&s, -900, "ARCO SHOP #12", start + 1);
        let beta = seed_txn(&s, -900, "Beta Place C", start + 2);
        seed_txn(&s, -900, "Gamma Store", start + 3);
        let ids: Vec<String> = s
            .preview_rule("arco shop* | beta place c")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![beta, arco]);
        assert!(s.preview_rule(" | ").unwrap().is_empty());
        // Nothing matches the bare words, so each alternative is offered wrapped.
        assert_eq!(
            s.suggest_contains_pattern("arco | beta").unwrap(),
            Some(("*arco* | *beta*".into(), 2))
        );
        // One alternative already matching means no suggestion.
        assert_eq!(
            s.suggest_contains_pattern("arco shop* | beta").unwrap(),
            None
        );
        assert_eq!(s.suggest_contains_pattern("*arco* | *beta*").unwrap(), None);
    }

    #[test]
    fn rule_pattern_without_wildcards_is_exact() {
        let (_d, s) = store();
        let cat = s.add_category("Groceries").unwrap();
        let (start, _) = month_bounds(2026, 9);
        let exact = seed_txn(&s, -900, "Costco", start + 1);
        let longer = seed_txn(&s, -900, "COSTCO WHSE #123", start + 2);
        s.add_rule(&Rule::pattern(
            "costco",
            RuleAction::Category,
            Some(cat.clone()),
            1,
        ))
        .unwrap();
        assert_eq!(s.auto_categorize_new().unwrap(), 1);
        let txns = s.list_transactions(false, None).unwrap();
        let by_id = |id: &str| {
            txns.iter()
                .find(|t| t.id == id)
                .unwrap()
                .category_id
                .clone()
        };
        assert_eq!(by_id(&exact).as_deref(), Some(cat.as_str()));
        assert_eq!(by_id(&longer), None);
    }

    #[test]
    fn suggest_contains_pattern_offers_wrapped_form() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        seed_txn(&s, -900, "HANDLE BAR CO", start + 1);
        seed_txn(&s, -900, "ACME HANDLERS", start + 2);
        // A half-typed "ends with" pattern matches nothing; the contains form would.
        assert_eq!(
            s.suggest_contains_pattern("*HANDL").unwrap(),
            Some(("*HANDL*".into(), 2))
        );
        assert_eq!(
            s.suggest_contains_pattern("handl").unwrap(),
            Some(("*handl*".into(), 2))
        );
        // Nothing to suggest when the pattern already matches, is already wrapped, or is hopeless.
        assert_eq!(s.suggest_contains_pattern("*handl*").unwrap(), None);
        assert_eq!(s.suggest_contains_pattern("handle*").unwrap(), None);
        assert_eq!(s.suggest_contains_pattern("*zzz").unwrap(), None);
        assert_eq!(s.suggest_contains_pattern("*").unwrap(), None);
        assert_eq!(s.suggest_contains_pattern("").unwrap(), None);
    }

    #[test]
    fn preview_rule_lists_matching_rows_newest_first() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let older = seed_txn(&s, -900, "COSTCO WHSE #123", start + 1);
        let newer = seed_txn(&s, -400, "Costco Gas", start + 5);
        seed_txn(&s, -100, "Cafe", start + 3);
        let ids: Vec<String> = s
            .preview_rule("costco*")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![newer, older]);
        assert!(s.preview_rule("").unwrap().is_empty());
        assert!(s.preview_rule("   ").unwrap().is_empty());
        assert!(s.preview_rule("nothing like this").unwrap().is_empty());
    }

    #[test]
    fn rules_first_match_wins() {
        let (_d, s) = store();
        let groceries = s.add_category("Groceries").unwrap();
        let other = s.add_category("Other").unwrap();
        s.add_rule(&Rule {
            id: String::new(),
            priority: 10,
            description_pattern: Some("costco*".into()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action: RuleAction::Category,
            category_id: Some(other),
            enabled: true,
        })
        .unwrap();
        s.add_rule(&Rule {
            id: String::new(),
            priority: 1,
            description_pattern: Some("*costco*".into()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action: RuleAction::Category,
            category_id: Some(groceries.clone()),
            enabled: true,
        })
        .unwrap();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        seed_txn(&s, -900, "COSTCO WHSE", start + 5);
        s.auto_categorize_new().unwrap();
        let txns = s.list_transactions(false, None).unwrap();
        assert_eq!(txns[0].category_id.as_deref(), Some(groceries.as_str()));
    }

    #[test]
    fn bulk_patch_two_transactions() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let cat = s.add_category("Food").unwrap();
        let a = seed_txn(&s, -100, "A", start + 1);
        let b = seed_txn(&s, -200, "B", start + 2);
        s.patch_transactions(
            &[a.clone(), b.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        let txns = s.list_transactions(false, None).unwrap();
        assert_eq!(txns.len(), 2);
        assert!(txns
            .iter()
            .all(|t| t.category_id.as_deref() == Some(cat.as_str())));
    }

    #[test]
    fn normalize_payee_collapses_space() {
        assert_eq!(normalize_payee("  Uncle   Frank "), "uncle frank");
        assert_eq!(
            normalize_payee("SQ *COSTCO #442 SEATTLE WA"),
            "costco seattle"
        );
        assert_eq!(normalize_payee("COSTCO WHSE #0566"), "costco whse");
        assert_eq!(normalize_payee("SQ *COSTCO #1"), "costco");
        assert_eq!(normalize_payee("CHECKCARD 0904 WALMART"), "walmart");
        assert_eq!(normalize_payee("TST* Coffee Shop"), "coffee shop");
    }

    #[test]
    fn categorizing_applies_memory_to_similar_payee() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let groceries = s.add_category("Groceries").unwrap();
        let a = seed_txn(&s, -900, "COSTCO", start + 1);
        let b = seed_txn(&s, -800, "SQ *COSTCO #1", start + 2);
        let extra = s
            .patch_transactions(
                &[a.clone()],
                &TxnPatch {
                    category_id: Some(Some(groceries.clone())),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(extra, 1);
        let ta = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == a)
            .unwrap();
        let tb = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == b)
            .unwrap();
        assert_eq!(ta.category_id.as_deref(), Some(groceries.as_str()));
        assert_eq!(tb.category_id.as_deref(), Some(groceries.as_str()));
        assert_eq!(categorized_by(&s, &a).as_deref(), Some("user"));
        assert_eq!(categorized_by(&s, &b).as_deref(), Some("memory"));
    }

    #[test]
    fn rules_beat_payee_memory() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let groceries = s.add_category("Groceries").unwrap();
        let household = s.add_category("Household").unwrap();
        let a = seed_txn(&s, -900, "COSTCO", start + 1);
        let b = seed_txn(&s, -800, "SQ *COSTCO #1", start + 2);
        s.patch_transactions(
            &[a.clone()],
            &TxnPatch {
                category_id: Some(Some(groceries.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        s.add_rule(&Rule {
            id: String::new(),
            priority: 1,
            description_pattern: Some("*costco*".into()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action: RuleAction::Category,
            category_id: Some(household.clone()),
            enabled: true,
        })
        .unwrap();
        s.auto_categorize_new().unwrap();
        let ta = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == a)
            .unwrap();
        let tb = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == b)
            .unwrap();
        assert_eq!(ta.category_id.as_deref(), Some(groceries.as_str()));
        assert_eq!(categorized_by(&s, &a).as_deref(), Some("user"));
        assert_eq!(tb.category_id.as_deref(), Some(household.as_str()));
        assert_eq!(categorized_by(&s, &b).as_deref(), Some("rule"));
    }

    #[test]
    fn user_category_is_not_overwritten() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let groceries = s.add_category("Groceries").unwrap();
        let other = s.add_category("Other").unwrap();
        let a = seed_txn(&s, -400, "Starbucks", start + 1);
        s.patch_transactions(
            &[a.clone()],
            &TxnPatch {
                category_id: Some(Some(groceries.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        s.add_rule(&Rule {
            id: String::new(),
            priority: 1,
            description_pattern: Some("*starbucks*".into()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action: RuleAction::Category,
            category_id: Some(other),
            enabled: true,
        })
        .unwrap();
        s.auto_categorize_new().unwrap();
        let ta = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == a)
            .unwrap();
        assert_eq!(ta.category_id.as_deref(), Some(groceries.as_str()));
        assert_eq!(categorized_by(&s, &a).as_deref(), Some("user"));
    }

    #[test]
    fn clearing_category_stays_uncategorized() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let groceries = s.add_category("Groceries").unwrap();
        let a = seed_txn(&s, -900, "COSTCO", start + 1);
        let b = seed_txn(&s, -800, "SQ *COSTCO #1", start + 2);
        s.patch_transactions(
            &[a.clone()],
            &TxnPatch {
                category_id: Some(Some(groceries.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        s.patch_transactions(
            &[a.clone()],
            &TxnPatch {
                category_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        s.auto_categorize_new().unwrap();
        let ta = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == a)
            .unwrap();
        let tb = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == b)
            .unwrap();
        assert!(ta.category_id.is_none());
        assert_eq!(categorized_by(&s, &a).as_deref(), Some("user"));
        assert_eq!(tb.category_id.as_deref(), Some(groceries.as_str()));
    }

    #[test]
    fn category_description_roundtrip() {
        let (_d, s) = store();
        let id = s.add_category("Food").unwrap();
        assert_eq!(s.list_categories().unwrap()[0].description, None);
        s.set_category_description(&id, "  Groceries and restaurants ")
            .unwrap();
        assert_eq!(
            s.list_categories().unwrap()[0].description.as_deref(),
            Some("Groceries and restaurants")
        );
        s.set_category_description(&id, "   ").unwrap();
        assert_eq!(s.list_categories().unwrap()[0].description, None);
        assert!(s.set_category_description("missing", "x").is_err());
    }

    #[test]
    fn delete_rule_removes_it() {
        let (_d, s) = store();
        let cat = s.add_category("Food").unwrap();
        let rid = s
            .add_rule(&Rule {
                id: String::new(),
                priority: 1,
                description_pattern: Some("cafe".into()),
                description_regex: None,
                amount_min_cents: None,
                amount_max_cents: None,
                account_id: None,
                action: RuleAction::Category,
                category_id: Some(cat),
                enabled: true,
            })
            .unwrap();
        assert_eq!(s.list_rules().unwrap().len(), 1);
        s.delete_rule(&rid).unwrap();
        assert!(s.list_rules().unwrap().is_empty());
        assert!(s.delete_rule(&rid).is_err());
    }

    #[test]
    fn typeahead_exact_and_unique_prefix() {
        let cats = vec![
            Category {
                id: "1".into(),
                name: "Groceries".into(),
                description: None,
                parent_id: None,
                parent_name: None,
                in_budget: true,
                parent_in_budget: true,
                send_to_ai: true,
                parent_send_to_ai: true,
            },
            Category {
                id: "2".into(),
                name: "Gas".into(),
                description: None,
                parent_id: None,
                parent_name: None,
                in_budget: true,
                parent_in_budget: true,
                send_to_ai: true,
                parent_send_to_ai: true,
            },
            Category {
                id: "3".into(),
                name: "Gifts".into(),
                description: None,
                parent_id: None,
                parent_name: None,
                in_budget: true,
                parent_in_budget: true,
                send_to_ai: true,
                parent_send_to_ai: true,
            },
        ];
        assert_eq!(
            match_category_typeahead("groceries", &cats).map(|c| c.id.as_str()),
            Some("1")
        );
        assert_eq!(
            match_category_typeahead("groc", &cats).map(|c| c.id.as_str()),
            Some("1")
        );
        assert_eq!(
            match_category_typeahead("  Gas  ", &cats).map(|c| c.id.as_str()),
            Some("2")
        );
        assert_eq!(
            match_category_typeahead("ga", &cats).map(|c| c.id.as_str()),
            Some("2")
        );
        assert!(match_category_typeahead("g", &cats).is_none());
        assert!(match_category_typeahead("", &cats).is_none());
        assert!(match_category_typeahead("rent", &cats).is_none());
    }

    #[test]
    fn add_category_rejects_blank() {
        let (_d, s) = store();
        assert!(s.add_category("").is_err());
        assert!(s.add_category("   ").is_err());
        assert!(s.list_categories().unwrap().is_empty());
    }

    #[test]
    fn move_category_swaps_neighbors_and_stops_at_ends() {
        let (_d, s) = store();
        let a = s.add_category("A").unwrap();
        let b = s.add_category("B").unwrap();
        let c = s.add_category("C").unwrap();
        let names = |s: &Store| -> Vec<String> {
            s.list_categories()
                .unwrap()
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        assert_eq!(names(&s), ["A", "B", "C"]);
        assert!(s.move_category(&c, -1).unwrap());
        assert_eq!(names(&s), ["A", "C", "B"]);
        assert!(s.move_category(&a, 1).unwrap());
        assert_eq!(names(&s), ["C", "A", "B"]);
        assert!(!s.move_category(&c, -1).unwrap());
        assert!(!s.move_category(&b, 1).unwrap());
        assert_eq!(names(&s), ["C", "A", "B"]);
        assert!(s.move_category("nope", 1).is_err());
        // A category added afterwards lands at the end.
        s.add_category("D").unwrap();
        assert_eq!(names(&s), ["C", "A", "B", "D"]);
    }

    #[test]
    fn rename_and_delete_category() {
        let (_d, s) = store();
        let id = s.add_category("  Food  ").unwrap();
        assert_eq!(s.list_categories().unwrap()[0].name, "Food");
        s.rename_category(&id, "Groceries").unwrap();
        assert_eq!(s.list_categories().unwrap()[0].name, "Groceries");
        assert!(s.rename_category(&id, "  ").is_err());

        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        s.set_budget(&id, y, m, 10000).unwrap();
        s.add_rule(&Rule {
            id: String::new(),
            priority: 1,
            description_pattern: Some("cafe".into()),
            description_regex: None,
            amount_min_cents: None,
            amount_max_cents: None,
            account_id: None,
            action: RuleAction::Category,
            category_id: Some(id.clone()),
            enabled: true,
        })
        .unwrap();
        let txn = seed_txn(&s, -100, "Cafe", start + 1);
        s.patch_transactions(
            &[txn.clone()],
            &TxnPatch {
                category_id: Some(Some(id.clone())),
                ..Default::default()
            },
        )
        .unwrap();

        s.delete_category(&id).unwrap();
        assert!(s.list_categories().unwrap().is_empty());
        assert!(s.list_rules().unwrap().is_empty());
        let txns = s.list_transactions(false, None).unwrap();
        assert!(txns[0].category_id.is_none());
        assert!(s.month_budget(y, m).unwrap().is_empty());
    }

    /// Names in `list_categories` order, children shown by their full label.
    fn labels(s: &Store) -> Vec<String> {
        s.list_categories()
            .unwrap()
            .into_iter()
            .map(|c| c.label())
            .collect()
    }

    #[test]
    fn subcategories_nest_one_level_and_list_under_their_parent() {
        let (_d, s) = store();
        let b = s.add_category("B").unwrap();
        let a = s.add_category("A").unwrap();
        let x = s.add_subcategory(&a, "x").unwrap();
        s.add_subcategory(&b, "y").unwrap();
        s.add_subcategory(&a, "z").unwrap();
        assert_eq!(labels(&s), ["B", "B › y", "A", "A › x", "A › z"]);

        let cats = s.list_categories().unwrap();
        let child = cats.iter().find(|c| c.id == x).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(a.as_str()));
        assert_eq!(child.parent_name.as_deref(), Some("A"));
        assert!(child.in_budget && child.parent_in_budget && child.is_budgeted());

        // One level only, and the parent has to exist.
        assert!(s.add_subcategory(&x, "deeper").is_err());
        assert!(s.add_subcategory("nope", "orphan").is_err());
        assert!(s.add_subcategory(&a, "  ").is_err());
    }

    #[test]
    fn move_category_stays_within_siblings() {
        let (_d, s) = store();
        let p = s.add_category("P").unwrap();
        let c1 = s.add_subcategory(&p, "c1").unwrap();
        let c2 = s.add_subcategory(&p, "c2").unwrap();
        let q = s.add_category("Q").unwrap();
        assert_eq!(labels(&s), ["P", "P › c1", "P › c2", "Q"]);

        assert!(s.move_category(&c2, -1).unwrap());
        assert_eq!(labels(&s), ["P", "P › c2", "P › c1", "Q"]);
        // A child at the end of its siblings never crosses into the top level.
        assert!(!s.move_category(&c1, 1).unwrap());
        assert!(!s.move_category(&c2, -1).unwrap());
        // A parent moves with its children.
        assert!(s.move_category(&q, -1).unwrap());
        assert_eq!(labels(&s), ["Q", "P", "P › c2", "P › c1"]);
    }

    #[test]
    fn set_category_parent_moves_one_level_only() {
        let (_d, s) = store();
        let a = s.add_category("A").unwrap();
        let b = s.add_category("B").unwrap();
        let x = s.add_subcategory(&a, "x").unwrap();

        assert!(s.set_category_parent(&a, Some(&a)).is_err());
        assert!(s.set_category_parent(&b, Some(&x)).is_err()); // x is a child
        assert!(s.set_category_parent(&a, Some(&b)).is_err()); // A has children
        assert!(s.set_category_parent("nope", None).is_err());

        s.set_category_parent(&b, Some(&a)).unwrap();
        assert_eq!(labels(&s), ["A", "A › x", "A › B"]);
        s.set_category_parent(&x, None).unwrap();
        assert_eq!(labels(&s), ["A", "A › B", "x"]);
    }

    #[test]
    fn delete_parent_cascades_to_children() {
        let (_d, s) = store();
        let p = s.add_category("Investment").unwrap();
        let c = s.add_subcategory(&p, "Fees").unwrap();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        s.set_budget(&c, y, m, 500).unwrap();
        s.add_rule(&Rule::pattern(
            "*fee*",
            RuleAction::Category,
            Some(c.clone()),
            1,
        ))
        .unwrap();
        let txn = seed_txn(&s, -100, "Broker fee", start + 1);
        s.patch_transactions(
            &[txn.clone()],
            &TxnPatch {
                category_id: Some(Some(c.clone())),
                ..Default::default()
            },
        )
        .unwrap();

        s.delete_category(&p).unwrap();
        assert!(s.list_categories().unwrap().is_empty());
        assert!(s.list_rules().unwrap().is_empty());
        assert!(s.list_transactions(false, None).unwrap()[0]
            .category_id
            .is_none());
        let memory: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM payee_memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(memory, 0);
        assert!(s.delete_category(&c).is_err());
    }

    #[test]
    fn month_budget_rolls_children_into_parent() {
        let (_d, s) = store();
        let p = s.add_category("Food").unwrap();
        let c = s.add_subcategory(&p, "Dining").unwrap();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        for (cat, amount, desc) in [(&p, -1000, "Market"), (&c, -250, "Cafe")] {
            let id = seed_txn(&s, amount, desc, start + 1);
            s.patch_transactions(
                &[id],
                &TxnPatch {
                    category_id: Some(Some(cat.clone())),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        s.set_budget(&p, y, m, 1200).unwrap();

        let rows = s.month_budget(y, m).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].category_name, "Food");
        assert_eq!(rows[0].parent_id, None);
        assert_eq!(rows[0].spent_cents, 1250);
        assert_eq!(rows[0].cap_cents, 1200);
        assert_eq!(rows[1].category_name, "Dining");
        assert_eq!(rows[1].parent_id.as_deref(), Some(p.as_str()));
        assert_eq!(rows[1].spent_cents, 250);
        assert!(rows.iter().all(|r| r.in_budget));
    }

    #[test]
    fn off_budget_category_has_no_cap_and_child_inherits() {
        let (_d, s) = store();
        let p = s.add_category("Investment").unwrap();
        let c = s.add_subcategory(&p, "Fees").unwrap();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let txn = seed_txn(&s, -300, "Broker fee", start + 1);
        s.patch_transactions(
            &[txn.clone()],
            &TxnPatch {
                category_id: Some(Some(c.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        s.set_budget(&c, y, m, 100).unwrap();

        // An off-budget child keeps its spend but loses its cap and never rolls up.
        s.set_category_in_budget(&c, false).unwrap();
        let rows = s.month_budget(y, m).unwrap();
        assert_eq!(rows[0].spent_cents, 0);
        assert!(rows[0].in_budget);
        assert!(!rows[1].in_budget);
        assert_eq!(rows[1].cap_cents, 0);
        assert_eq!(rows[1].spent_cents, 300);
        // Back on, the stored cap is in force again.
        s.set_category_in_budget(&c, true).unwrap();
        assert_eq!(s.month_budget(y, m).unwrap()[1].cap_cents, 100);

        // Turning the parent off unticks the child too, and the child can't come back alone.
        s.set_category_in_budget(&p, false).unwrap();
        let cats = s.list_categories().unwrap();
        let child = cats.iter().find(|cat| cat.id == c).unwrap();
        assert!(!child.in_budget && !child.parent_in_budget && !child.is_budgeted());
        assert!(s.month_budget(y, m).unwrap().iter().all(|r| !r.in_budget));
        assert!(s.set_category_in_budget(&c, true).is_err());
        // Turning the parent back on leaves the child unticked.
        s.set_category_in_budget(&p, true).unwrap();
        let rows = s.month_budget(y, m).unwrap();
        assert!(rows[0].in_budget);
        assert!(!rows[1].in_budget);
        s.set_category_in_budget(&c, true).unwrap();
        assert!(s.month_budget(y, m).unwrap().iter().all(|r| r.in_budget));
        // The row still takes transactions and still shows in the list the AI sees.
        assert_eq!(s.list_categories().unwrap().len(), 2);
        assert!(s.list_categories().unwrap().iter().all(|c| c.send_to_ai));
        assert!(s.set_category_in_budget("nope", false).is_err());
    }

    #[test]
    fn send_to_ai_defaults_on_and_can_be_toggled() {
        let (_d, s) = store();
        let id = s.add_category("Other").unwrap();
        assert!(s.list_categories().unwrap()[0].send_to_ai);
        s.set_category_send_to_ai(&id, false).unwrap();
        assert!(!s.list_categories().unwrap()[0].send_to_ai);
        s.set_category_send_to_ai(&id, true).unwrap();
        assert!(s.list_categories().unwrap()[0].send_to_ai);
        assert!(s.set_category_send_to_ai("nope", false).is_err());
        // Unticking a parent unticks its children. Turning the parent back on leaves them off.
        let p = s.add_category("Investment").unwrap();
        let fees = s.add_subcategory(&p, "Fees").unwrap();
        let tax = s.add_subcategory(&p, "Tax").unwrap();
        s.set_category_send_to_ai(&fees, false).unwrap();
        s.set_category_send_to_ai(&p, false).unwrap();
        let cats = s.list_categories().unwrap();
        let parent = cats.iter().find(|cat| cat.id == p).unwrap();
        let fees_row = cats.iter().find(|cat| cat.id == fees).unwrap();
        let tax_row = cats.iter().find(|cat| cat.id == tax).unwrap();
        assert!(!parent.send_to_ai && parent.parent_send_to_ai);
        assert!(!fees_row.send_to_ai && !fees_row.parent_send_to_ai && !fees_row.is_sent_to_ai());
        assert!(!tax_row.send_to_ai && !tax_row.is_sent_to_ai());
        assert!(s.set_category_send_to_ai(&fees, true).is_err());
        s.set_category_send_to_ai(&p, true).unwrap();
        let cats = s.list_categories().unwrap();
        assert!(cats.iter().find(|cat| cat.id == p).unwrap().is_sent_to_ai());
        assert!(!cats.iter().find(|cat| cat.id == fees).unwrap().send_to_ai);
        assert!(!cats.iter().find(|cat| cat.id == tax).unwrap().send_to_ai);
        s.set_category_send_to_ai(&tax, true).unwrap();
        assert!(s
            .list_categories()
            .unwrap()
            .iter()
            .find(|cat| cat.id == tax)
            .unwrap()
            .is_sent_to_ai());
    }

    #[test]
    fn query_by_parent_includes_children_and_names_use_full_label() {
        let (_d, s) = store();
        let p = s.add_category("Food").unwrap();
        let c = s.add_subcategory(&p, "Dining").unwrap();
        let other = s.add_category("Gas").unwrap();
        let (start, _) = month_bounds(2026, 9);
        for (cat, desc) in [(&p, "Market"), (&c, "Cafe"), (&other, "Arco")] {
            let id = seed_txn(&s, -100, desc, start + 1);
            s.patch_transactions(
                &[id],
                &TxnPatch {
                    category_id: Some(Some(cat.clone())),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let by = |cat: &str| {
            let mut v: Vec<String> = s
                .query_transactions(&TxnQuery {
                    category_id: Some(cat.into()),
                    ..Default::default()
                })
                .unwrap()
                .into_iter()
                .map(|t| t.payee)
                .collect();
            v.sort();
            v
        };
        assert_eq!(by(&p), ["Cafe", "Market"]);
        assert_eq!(by(&c), ["Cafe"]);
        assert_eq!(by(&other), ["Arco"]);

        let txns = s.list_transactions(false, None).unwrap();
        let name = |payee: &str| {
            txns.iter()
                .find(|t| t.payee == payee)
                .unwrap()
                .category_name
                .clone()
        };
        assert_eq!(name("Cafe").as_deref(), Some("Food › Dining"));
        assert_eq!(name("Market").as_deref(), Some("Food"));
    }

    #[test]
    fn typeahead_reaches_subcategories() {
        let (_d, s) = store();
        let p = s.add_category("Investment").unwrap();
        let fees = s.add_subcategory(&p, "Fees").unwrap();
        let interest = s.add_subcategory(&p, "Interest").unwrap();
        let cats = s.list_categories().unwrap();
        let hit = |typed: &str| match_category_typeahead(typed, &cats).map(|c| c.id.clone());
        // Name before label: the parent still wins its own prefix.
        assert_eq!(hit("investment"), Some(p.clone()));
        assert_eq!(hit("inv"), Some(p.clone()));
        // Children by their own name, a prefix of it, or the full label.
        assert_eq!(hit("fees"), Some(fees.clone()));
        assert_eq!(hit("int"), Some(interest.clone()));
        assert_eq!(hit("Investment › Fees"), Some(fees.clone()));
        assert_eq!(hit("investment › i"), Some(interest));
        // Ambiguous stays ambiguous.
        assert_eq!(hit("i"), None);
    }

    #[test]
    fn query_search_matches_payee_notes_and_amount() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let a = seed_txn(&s, -1250, "COSTCO WHSE", start + 5);
        let b = seed_txn(&s, -4200, "Shell Oil", start + 6);
        s.patch_transactions(
            &[b.clone()],
            &TxnPatch {
                notes: Some(Some("road trip snacks".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let q = |search: &str| {
            s.query_transactions(&TxnQuery {
                search: search.into(),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect::<Vec<_>>()
        };
        assert_eq!(q("costco"), [a.clone()]);
        assert_eq!(q("SNACKS"), [b.clone()]);
        assert_eq!(q("12.50"), [a.clone()]);
        assert_eq!(q("42"), [b.clone()]);
        assert!(q("zzz").is_empty());
        assert_eq!(q("  ").len(), 2);
    }

    #[test]
    fn query_filters_by_account_and_month() {
        let (_d, s) = store();
        let (aug, _) = month_bounds(2026, 8);
        let (sep, _) = month_bounds(2026, 9);
        let a = seed_txn(&s, -100, "August thing", aug + 5);
        let b = seed_txn(&s, -200, "September thing", sep + 5);
        let sep_rows = s
            .query_transactions(&TxnQuery {
                month: Some((2026, 9)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(sep_rows.iter().map(|t| &t.id).collect::<Vec<_>>(), [&b]);
        let acct = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == a)
            .unwrap()
            .account_id;
        let acct_rows = s
            .query_transactions(&TxnQuery {
                account_id: Some(acct),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(acct_rows.iter().map(|t| &t.id).collect::<Vec<_>>(), [&a]);
        assert_eq!(s.list_months().unwrap(), [(2026, 9), (2026, 8)]);
    }

    #[test]
    fn uncategorized_count_skips_excluded_and_categorized() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let food = s.add_category("Food").unwrap();
        let a = seed_txn(&s, -100, "One", start + 1);
        let b = seed_txn(&s, -100, "Two", start + 2);
        seed_txn(&s, -100, "Three", start + 3);
        assert_eq!(s.uncategorized_count().unwrap(), 3);
        s.patch_transactions(
            &[a],
            &TxnPatch {
                category_id: Some(Some(food)),
                ..Default::default()
            },
        )
        .unwrap();
        s.patch_transactions(
            &[b],
            &TxnPatch {
                excluded: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.uncategorized_count().unwrap(), 1);
    }

    #[test]
    fn caps_carry_forward_until_changed() {
        let (_d, s) = store();
        let food = s.add_category("Food").unwrap();
        let gas = s.add_category("Gas").unwrap();
        s.set_budget(&food, 2026, 8, 40000).unwrap();
        s.set_budget(&gas, 2026, 8, 10000).unwrap();
        s.set_budget(&gas, 2026, 10, 12000).unwrap();
        let cap = |y: i32, m: u32, name: &str| {
            s.month_budget(y, m)
                .unwrap()
                .into_iter()
                .find(|r| r.category_name == name)
                .unwrap()
                .cap_cents
        };
        // Before any cap was set there is none.
        assert_eq!(cap(2026, 7, "Food"), 0);
        // A cap carries into later months, across the year boundary too.
        assert_eq!(cap(2026, 8, "Food"), 40000);
        assert_eq!(cap(2026, 9, "Food"), 40000);
        assert_eq!(cap(2027, 3, "Food"), 40000);
        // A later change takes over from its month on, leaving earlier months alone.
        assert_eq!(cap(2026, 9, "Gas"), 10000);
        assert_eq!(cap(2026, 10, "Gas"), 12000);
        assert_eq!(cap(2026, 11, "Gas"), 12000);
        // Clearing a cap (0) also carries forward.
        s.set_budget(&gas, 2026, 12, 0).unwrap();
        assert_eq!(cap(2026, 11, "Gas"), 12000);
        assert_eq!(cap(2027, 1, "Gas"), 0);
    }

    #[test]
    fn delete_connection_removes_its_transactions() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        seed_txn(&s, -100, "Keep", start + 1);
        seed_txn(&s, -100, "Drop", start + 2);
        let conns = s.list_connections().unwrap();
        assert_eq!(conns.len(), 2);
        let victim = &conns[1].id;
        s.delete_connection(victim).unwrap();
        assert_eq!(s.list_connections().unwrap().len(), 1);
        let left = s.list_transactions(false, None).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].payee, "Keep");
        assert!(s.delete_connection(victim).is_err());
    }

    #[test]
    fn transfer_rule_marks_rows_and_skips_review() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let cat = s.add_category("Savings").unwrap();
        s.set_budget(&cat, y, m, 10000).unwrap();
        let id = seed_txn(&s, -50000, "ONLINE TRANSFER TO SAVINGS", start + 5);
        seed_txn(&s, -1200, "Cafe", start + 6);
        s.add_rule(&Rule::pattern(
            "*transfer to savings",
            RuleAction::Transfer,
            None,
            1,
        ))
        .unwrap();
        assert_eq!(s.auto_categorize_new().unwrap(), 1);
        let txn = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert!(txn.is_transfer);
        assert!(txn.category_id.is_none());
        assert_eq!(s.uncategorized_count().unwrap(), 1);
        // Even if categorized, a transfer never counts toward a cap.
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 0);
        // Re-running is idempotent.
        assert_eq!(s.auto_categorize_new().unwrap(), 0);
    }

    #[test]
    fn payment_marks_are_transfers_and_skip_spend() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let cat = s.add_category("Bills").unwrap();
        s.set_budget(&cat, y, m, 100000).unwrap();
        let id = seed_txn(&s, -45000, "CHASE CARD PAYMENT", start + 3);
        let get = || {
            s.list_transactions(false, None)
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
        };

        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                payment: Some(Some(PaymentKind::Card)),
                ..Default::default()
            },
        )
        .unwrap();
        let txn = get();
        assert!(txn.is_transfer);
        assert_eq!(txn.payment, Some(PaymentKind::Card));
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 0);
        assert_eq!(s.uncategorized_count().unwrap(), 0);

        // Switching the kind keeps it a transfer.
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                payment: Some(Some(PaymentKind::Loan)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get().payment, Some(PaymentKind::Loan));
        assert!(get().is_transfer);

        // Un-marking the transfer forgets the kind and the row counts again.
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                transfer_account_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let txn = get();
        assert!(!txn.is_transfer);
        assert_eq!(txn.payment, None);
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 45000);
    }

    #[test]
    fn payment_rules_mark_rows_and_skip_review() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let card = seed_txn(&s, -45000, "CHASE CARD PAYMENT", start + 3);
        let loan = seed_txn(&s, -30000, "TOYOTA FINANCIAL AUTOPAY", start + 4);
        let other = seed_txn(&s, -1200, "Cafe", start + 5);
        s.add_rule(&Rule::pattern(
            "*card payment",
            RuleAction::CardPayment,
            None,
            1,
        ))
        .unwrap();
        s.add_rule(&Rule::pattern(
            "toyota financial*",
            RuleAction::LoanPayment,
            None,
            2,
        ))
        .unwrap();
        assert_eq!(s.auto_categorize_new().unwrap(), 2);
        let txns = s.list_transactions(false, None).unwrap();
        let get = |id: &str| txns.iter().find(|t| t.id == id).unwrap();
        assert_eq!(get(&card).payment, Some(PaymentKind::Card));
        assert!(get(&card).is_transfer);
        assert_eq!(get(&loan).payment, Some(PaymentKind::Loan));
        assert!(get(&loan).is_transfer);
        assert_eq!(get(&other).payment, None);
        assert!(!get(&other).is_transfer);
        assert_eq!(s.uncategorized_count().unwrap(), 1);
        // Re-running is idempotent.
        assert_eq!(s.auto_categorize_new().unwrap(), 0);
    }

    #[test]
    fn payment_kind_and_rule_action_round_trip() {
        for k in [PaymentKind::Card, PaymentKind::Loan] {
            assert_eq!(PaymentKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(PaymentKind::parse("nope"), None);
        for a in [RuleAction::CardPayment, RuleAction::LoanPayment] {
            assert_eq!(RuleAction::parse(a.as_str()), a);
        }
    }

    #[test]
    fn exclude_rule_excludes_matching_rows() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let id = seed_txn(&s, -999, "CREDIT CARD PAYMENT", start + 5);
        let other = seed_txn(&s, -100, "Cafe", start + 6);
        s.add_rule(&Rule::pattern(
            "credit card payment",
            RuleAction::Exclude,
            None,
            1,
        ))
        .unwrap();
        assert_eq!(s.auto_categorize_new().unwrap(), 1);
        let txns = s.list_transactions(false, None).unwrap();
        assert!(txns.iter().find(|t| t.id == id).unwrap().excluded);
        assert!(!txns.iter().find(|t| t.id == other).unwrap().excluded);
        assert_eq!(s.uncategorized_count().unwrap(), 1);
    }

    #[test]
    fn query_excluded_only_lists_just_excluded_rows() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let ex = seed_txn(&s, -500, "Reimbursed", start + 1);
        let kept = seed_txn(&s, -100, "Cafe", start + 2);
        s.patch_transactions(
            &[ex.clone()],
            &TxnPatch {
                excluded: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let rows = s
            .query_transactions(&TxnQuery {
                excluded_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, ex);
        assert!(rows[0].excluded);
        assert!(s
            .list_transactions(false, None)
            .unwrap()
            .iter()
            .any(|t| t.id == kept));
    }

    #[test]
    fn reverse_patch_restores_the_row() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let id = seed_txn(&s, -4200, "COSTCO", start + 3);
        let cat = s.add_category("Groceries").unwrap();
        let get = |s: &Store| {
            s.list_transactions(false, None)
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
        };
        let before = get(&s);

        // Mark as card payment, then undo: transfer flag and kind both come back off.
        let fwd = TxnPatch {
            transfer_account_id: Some(Some("manual".into())),
            payment: Some(Some(PaymentKind::Card)),
            income: Some(false),
            excluded: Some(false),
            ..Default::default()
        };
        let back = fwd.reverse_for(&before);
        assert_eq!(back.transfer_account_id, Some(None));
        assert_eq!(back.payment, Some(None));
        assert_eq!(back.category_id, None);
        s.patch_transactions(&[id.clone()], &fwd).unwrap();
        assert_eq!(get(&s).payment, Some(PaymentKind::Card));
        s.patch_transactions(&[id.clone()], &back).unwrap();
        let after = get(&s);
        assert!(!after.is_transfer);
        assert_eq!(after.payment, None);

        // Field edits round-trip too, including clearing a note and a category.
        s.patch_transactions(
            &[id.clone()],
            &TxnPatch {
                notes: Some(Some("lunch".into())),
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        let noted = get(&s);
        let fwd = TxnPatch {
            payee: Some("Costco Wholesale".into()),
            notes: Some(None),
            amount_cents: Some(-100),
            posted_at: Some(start + 9),
            category_id: Some(None),
            ..Default::default()
        };
        let back = fwd.reverse_for(&noted);
        s.patch_transactions(&[id.clone()], &fwd).unwrap();
        s.patch_transactions(&[id.clone()], &back).unwrap();
        let restored = get(&s);
        assert_eq!(restored.payee, "COSTCO");
        assert_eq!(restored.notes.as_deref(), Some("lunch"));
        assert_eq!(restored.amount_cents, -4200);
        assert_eq!(restored.posted_at, start + 3);
        assert_eq!(restored.category_id, Some(cat));
    }

    #[test]
    fn income_rule_and_manual_income_flag() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let pay = seed_txn(&s, 250000, "ACME CORP PAYROLL", start + 1);
        let refund = seed_txn(&s, 1500, "Refund", start + 2);
        seed_txn(&s, -300, "Cafe", start + 3);
        s.add_rule(&Rule::pattern("*payroll", RuleAction::Income, None, 1))
            .unwrap();
        assert_eq!(s.auto_categorize_new().unwrap(), 1);
        let by_id = |id: &str| {
            s.list_transactions(false, None)
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
        };
        assert!(by_id(&pay).is_income);
        assert!(!by_id(&refund).is_income);
        assert_eq!(s.month_income(y, m).unwrap(), 250000);
        // Paycheck leaves the review list; refund and cafe stay.
        assert_eq!(s.uncategorized_count().unwrap(), 2);
        assert_eq!(s.list_transactions(true, None).unwrap().len(), 2);

        s.patch_transactions(
            &[refund.clone()],
            &TxnPatch {
                income: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(by_id(&refund).is_income);
        assert_eq!(s.month_income(y, m).unwrap(), 251500);
        s.patch_transactions(
            &[pay.clone()],
            &TxnPatch {
                income: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!by_id(&pay).is_income);
        assert_eq!(s.month_income(y, m).unwrap(), 1500);
    }

    #[test]
    fn categorize_rule_requires_category() {
        let (_d, s) = store();
        assert!(s
            .add_rule(&Rule::pattern("x", RuleAction::Category, None, 1))
            .is_err());
        assert!(s
            .add_rule(&Rule::pattern(
                "x",
                RuleAction::Category,
                Some(String::new()),
                1
            ))
            .is_err());
        let cat = s.add_category("Food").unwrap();
        s.add_rule(&Rule::pattern(
            "x",
            RuleAction::Category,
            Some(cat.clone()),
            1,
        ))
        .unwrap();
        let rules = s.list_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].action, RuleAction::Category);
        assert_eq!(rules[0].category_id.as_deref(), Some(cat.as_str()));
    }

    #[test]
    fn flag_rules_round_trip_through_store() {
        let (_d, s) = store();
        s.add_rule(&Rule::pattern("a", RuleAction::Transfer, None, 1))
            .unwrap();
        s.add_rule(&Rule::pattern("b", RuleAction::Income, None, 2))
            .unwrap();
        s.add_rule(&Rule::pattern("c", RuleAction::Exclude, None, 3))
            .unwrap();
        let actions: Vec<RuleAction> = s.list_rules().unwrap().iter().map(|r| r.action).collect();
        assert_eq!(
            actions,
            vec![
                RuleAction::Transfer,
                RuleAction::Income,
                RuleAction::Exclude
            ]
        );
        assert!(s
            .list_rules()
            .unwrap()
            .iter()
            .all(|r| r.category_id.is_none()));
    }

    #[test]
    fn hidden_account_drops_out_everywhere_unless_filtered() {
        let (_d, s) = store();
        let (y, m) = (2026, 9);
        let (start, _) = month_bounds(y, m);
        let cat = s.add_category("Food").unwrap();
        s.set_budget(&cat, y, m, 10000).unwrap();
        // Each seed_txn call makes its own connection and account.
        let visible = seed_txn(&s, -1000, "Cafe", start + 1);
        let hidden = seed_txn(&s, -2000, "Cafe", start + 2);
        s.patch_transactions(
            &[visible.clone(), hidden.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        s.patch_transactions(
            &[hidden.clone()],
            &TxnPatch {
                category_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 1000);
        assert_eq!(s.uncategorized_count().unwrap(), 1);

        let accounts = s.list_accounts().unwrap();
        let hidden_acct = s
            .list_transactions(false, None)
            .unwrap()
            .into_iter()
            .find(|t| t.id == hidden)
            .unwrap()
            .account_id;
        assert!(accounts.iter().all(|a| !a.hidden));
        s.set_account_hidden(&hidden_acct, true).unwrap();
        assert!(s
            .list_accounts()
            .unwrap()
            .iter()
            .any(|a| a.id == hidden_acct && a.hidden));

        let all = s.list_transactions(false, None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, visible);
        assert_eq!(s.uncategorized_count().unwrap(), 0);
        assert_eq!(s.category_spent(&cat, y, m).unwrap(), 1000);
        assert_eq!(s.list_months().unwrap(), vec![(y, m)]);

        // Filtering by the hidden account still shows its rows.
        let only = s
            .query_transactions(&TxnQuery {
                account_id: Some(hidden_acct.clone()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].id, hidden);

        s.set_account_hidden(&hidden_acct, false).unwrap();
        assert_eq!(s.list_transactions(false, None).unwrap().len(), 2);
        assert!(s.set_account_hidden("nope", true).is_err());
    }

    #[test]
    fn create_rule_applies_to_every_match_including_user_rows() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let food = s.add_category("Food").unwrap();
        let groceries = s.add_category("Groceries").unwrap();
        let by_hand = seed_txn(&s, -900, "COSTCO WHSE #123", start + 1);
        let untouched = seed_txn(&s, -900, "COSTCO WHSE #456", start + 2);
        let other = seed_txn(&s, -100, "Cafe", start + 3);
        s.patch_transactions(
            &[by_hand.clone()],
            &TxnPatch {
                category_id: Some(Some(food.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(categorized_by(&s, &by_hand).as_deref(), Some("user"));

        let (id, n) = s
            .create_rule(&Rule::pattern(
                "*costco*",
                RuleAction::Category,
                Some(groceries.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(n, 2);
        assert!(s.list_rules().unwrap().iter().any(|r| r.id == id));
        let txns = s.list_transactions(false, None).unwrap();
        let cat = |id: &str| {
            txns.iter()
                .find(|t| t.id == id)
                .unwrap()
                .category_id
                .clone()
        };
        assert_eq!(cat(&by_hand).as_deref(), Some(groceries.as_str()));
        assert_eq!(cat(&untouched).as_deref(), Some(groceries.as_str()));
        assert_eq!(cat(&other), None);
        assert_eq!(categorized_by(&s, &by_hand).as_deref(), Some("rule"));

        // Rows already on the rule's category do not count as changed.
        let (_, again) = s
            .create_rule(&Rule::pattern(
                "costco whse #456",
                RuleAction::Category,
                Some(groceries.clone()),
                2,
            ))
            .unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn create_rule_respects_earlier_rules() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let groceries = s.add_category("Groceries").unwrap();
        let fuel = s.add_category("Fuel").unwrap();
        let gas = seed_txn(&s, -4000, "COSTCO GAS #9", start + 1);
        let whse = seed_txn(&s, -900, "COSTCO WHSE #1", start + 2);
        let (_, first) = s
            .create_rule(&Rule::pattern(
                "*costco*",
                RuleAction::Category,
                Some(groceries.clone()),
                1,
            ))
            .unwrap();
        assert_eq!(first, 2);
        // The earlier rule already claims both rows, so a later, narrower rule changes nothing.
        let (_, second) = s
            .create_rule(&Rule::pattern(
                "*costco gas*",
                RuleAction::Category,
                Some(fuel.clone()),
                2,
            ))
            .unwrap();
        assert_eq!(second, 0);
        let txns = s.list_transactions(false, None).unwrap();
        let cat = |id: &str| {
            txns.iter()
                .find(|t| t.id == id)
                .unwrap()
                .category_id
                .clone()
        };
        assert_eq!(cat(&gas).as_deref(), Some(groceries.as_str()));
        assert_eq!(cat(&whse).as_deref(), Some(groceries.as_str()));
    }

    #[test]
    fn create_rule_flag_actions_keep_category() {
        let (_d, s) = store();
        let (start, _) = month_bounds(2026, 9);
        let cat = s.add_category("Savings").unwrap();
        let xfer = seed_txn(&s, -50000, "ONLINE TRANSFER TO SAVINGS", start + 1);
        let pay = seed_txn(&s, 300000, "ACME CORP PAYROLL", start + 2);
        let fee = seed_txn(&s, -999, "CREDIT CARD PAYMENT", start + 3);
        s.patch_transactions(
            &[xfer.clone()],
            &TxnPatch {
                category_id: Some(Some(cat.clone())),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            s.create_rule(&Rule::pattern(
                "*transfer to savings",
                RuleAction::Transfer,
                None,
                1
            ))
            .unwrap()
            .1,
            1
        );
        assert_eq!(
            s.create_rule(&Rule::pattern("*payroll", RuleAction::Income, None, 2))
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            s.create_rule(&Rule::pattern(
                "credit card payment",
                RuleAction::Exclude,
                None,
                3
            ))
            .unwrap()
            .1,
            1
        );

        let txns = s.list_transactions(false, None).unwrap();
        let get = |id: &str| txns.iter().find(|t| t.id == id).unwrap();
        assert!(get(&xfer).is_transfer);
        assert_eq!(get(&xfer).category_id.as_deref(), Some(cat.as_str()));
        assert!(get(&pay).is_income);
        assert!(get(&fee).excluded);
        // Re-running the normal pass finds nothing new.
        assert_eq!(s.auto_categorize_new().unwrap(), 0);
    }

    #[test]
    fn create_rule_rejects_categorize_without_category() {
        let (_d, s) = store();
        assert!(s
            .create_rule(&Rule::pattern("x", RuleAction::Category, None, 1))
            .is_err());
        assert!(s.list_rules().unwrap().is_empty());
    }
}
