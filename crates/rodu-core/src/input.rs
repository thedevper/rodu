//! Validation of what callers send: item fields, patches, names and dates. Every surface (CLI,
//! MCP, HTTP) passes JSON-shaped input through here, so the rules live in one place.

use std::sync::LazyLock;

use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use time::Date;
use time::macros::format_description;

use crate::error::{Result, RoduError};
use crate::model::{ItemType, Priority};

/// One line of text people wrote: no control characters, Unicode line/paragraph separators or
/// invisible format characters (bidi marks and overrides, zero-width space, U+061C), any of which
/// could forge structure when the text is rendered for an agent. ZWNJ/ZWJ and emoji tag characters
/// (subdivision flags) stay allowed because emoji and some scripts need them.
static SINGLE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}]|\x{200C}|\x{200D}|[\x{E0020}-\x{E007F}])*$")
        .unwrap()
});

pub fn is_single_line(text: &str) -> bool {
    SINGLE_LINE.is_match(text)
}

pub const MAX_TITLE: usize = 300;
pub const MAX_BODY: usize = 100_000;
pub const MAX_ESTIMATE: f64 = 1000.0;

/// Trims and checks a one-line name of 1..=max characters.
pub fn check_name(name: &str, max: usize) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > max || !is_single_line(trimmed) {
        return Err(RoduError::invalid(format!("Name must be one line of 1-{max} characters")));
    }
    Ok(trimmed.to_string())
}

fn check_title(title: &str) -> std::result::Result<String, String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("must not be empty".into());
    }
    if trimmed.chars().count() > MAX_TITLE {
        return Err(format!("must be at most {MAX_TITLE} characters"));
    }
    if !is_single_line(trimmed) {
        return Err("must be one line without control characters".into());
    }
    Ok(trimmed.to_string())
}

/// A calendar date, YYYY-MM-DD, that actually exists.
pub fn check_date(text: &str) -> std::result::Result<String, String> {
    let shape = text.len() == 10
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    if !shape {
        return Err("expected YYYY-MM-DD".into());
    }
    Date::parse(text, format_description!("[year]-[month]-[day]"))
        .map(|_| text.to_string())
        .map_err(|_| "not a real date".into())
}

/// A date field from outside, as an error naming the field.
pub fn parse_date(text: &str, field: &str) -> Result<String> {
    check_date(text).map_err(|m| RoduError::invalid(format!("Invalid {field}: {m}")))
}

fn check_estimate(value: f64) -> std::result::Result<f64, String> {
    if !(0.0..=MAX_ESTIMATE).contains(&value) {
        return Err(format!("must be from 0 to {MAX_ESTIMATE}"));
    }
    Ok(value)
}

fn check_ref(value: &str) -> std::result::Result<String, String> {
    if value.is_empty() {
        return Err("must not be empty".into());
    }
    Ok(value.to_string())
}

/// Distinguishes a missing field (`None`) from an explicit null (`Some(None)`).
fn double_option<'de, T, D>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNewItem {
    title: String,
    #[serde(rename = "type")]
    item_type: Option<ItemType>,
    body: Option<String>,
    priority: Option<Priority>,
    assignee: Option<String>,
    parent: Option<String>,
    estimate: Option<f64>,
    due_at: Option<String>,
    cycle: Option<String>,
}

/// A validated new item. Serialized only to fingerprint idempotent requests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewItem {
    pub title: String,
    pub item_type: ItemType,
    pub body: String,
    pub priority: Priority,
    /// Principal name, id or "me".
    pub assignee: Option<String>,
    /// Parent item key (DEMO-3) or id.
    pub parent: Option<String>,
    pub estimate: Option<f64>,
    pub due_at: Option<String>,
    /// Cycle name in the same collection.
    pub cycle: Option<String>,
}

impl NewItem {
    /// A new task with just a title, for callers that build items in code.
    pub fn titled(title: impl Into<String>) -> serde_json::Value {
        serde_json::json!({ "title": title.into() })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPatch {
    title: Option<String>,
    #[serde(rename = "type")]
    item_type: Option<ItemType>,
    body: Option<String>,
    priority: Option<Priority>,
    #[serde(default, deserialize_with = "double_option")]
    assignee: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    parent: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    estimate: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    due_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    cycle: Option<Option<String>>,
}

/// A validated partial update. `Some(None)` clears a field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemPatch {
    pub title: Option<String>,
    pub item_type: Option<ItemType>,
    pub body: Option<String>,
    pub priority: Option<Priority>,
    pub assignee: Option<Option<String>>,
    pub parent: Option<Option<String>>,
    pub estimate: Option<Option<f64>>,
    pub due_at: Option<Option<String>>,
    pub cycle: Option<Option<String>>,
}

fn decode<T: DeserializeOwned>(input: &serde_json::Value, what: &str) -> Result<T> {
    if !input.is_object() {
        return Err(RoduError::invalid(format!("Invalid {what}: expected an object")));
    }
    serde_json::from_value(input.clone())
        .map_err(|e| RoduError::invalid(format!("Invalid {what}: {e}")))
}

fn field<T>(what: &str, name: &str, check: std::result::Result<T, String>) -> Result<T> {
    check.map_err(|m| RoduError::invalid(format!("Invalid {what}: {name}: {m}")))
}

fn body(what: &str, text: String) -> Result<String> {
    if text.chars().count() > MAX_BODY {
        return Err(RoduError::invalid(format!(
            "Invalid {what}: body: must be at most {MAX_BODY} characters"
        )));
    }
    Ok(text)
}

/// Validates one new item; `what` names it in errors, e.g. `items[2]`.
pub fn parse_new_item(input: &serde_json::Value, what: &str) -> Result<NewItem> {
    let raw: RawNewItem = decode(input, what)?;
    Ok(NewItem {
        title: field(what, "title", check_title(&raw.title))?,
        item_type: raw.item_type.unwrap_or(ItemType::Task),
        body: body(what, raw.body.unwrap_or_default())?,
        priority: raw.priority.unwrap_or(Priority::None),
        assignee: raw.assignee.map(|v| field(what, "assignee", check_ref(&v))).transpose()?,
        parent: raw.parent.map(|v| field(what, "parent", check_ref(&v))).transpose()?,
        estimate: raw.estimate.map(|v| field(what, "estimate", check_estimate(v))).transpose()?,
        due_at: raw.due_at.map(|v| field(what, "dueAt", check_date(&v))).transpose()?,
        cycle: raw.cycle.map(|v| field(what, "cycle", check_ref(&v))).transpose()?,
    })
}

fn nullable<T>(
    value: Option<Option<T>>,
    check: impl FnOnce(T) -> std::result::Result<T, String>,
    name: &str,
) -> Result<Option<Option<T>>> {
    match value {
        Some(Some(v)) => Ok(Some(Some(field("patch", name, check(v))?))),
        other => Ok(other),
    }
}

/// Validates a patch. Unknown fields are refused, so a typo is never silently ignored.
pub fn parse_patch(input: &serde_json::Value) -> Result<ItemPatch> {
    let raw: RawPatch = decode(input, "patch")?;
    Ok(ItemPatch {
        title: raw.title.map(|t| field("patch", "title", check_title(&t))).transpose()?,
        item_type: raw.item_type,
        body: raw.body.map(|b| body("patch", b)).transpose()?,
        priority: raw.priority,
        assignee: nullable(raw.assignee, |v| check_ref(&v), "assignee")?,
        parent: nullable(raw.parent, |v| check_ref(&v), "parent")?,
        estimate: nullable(raw.estimate, check_estimate, "estimate")?,
        due_at: nullable(raw.due_at, |v| check_date(&v), "dueAt")?,
        cycle: nullable(raw.cycle, |v| check_ref(&v), "cycle")?,
    })
}

impl ItemPatch {
    pub fn is_empty(&self) -> bool {
        *self == ItemPatch::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fills_defaults_and_trims_titles() {
        let item =
            parse_new_item(&json!({ "title": "  Crash on login ", "extra": 1 }), "item").unwrap();
        assert_eq!(item.title, "Crash on login");
        assert_eq!(item.item_type, ItemType::Task);
        assert_eq!(item.priority, Priority::None);
    }

    #[test]
    fn refuses_titles_that_could_forge_structure() {
        for title in ["", "two\nlines", "bidi \u{202E} override", "zero\u{200B}width"] {
            let err = parse_new_item(&json!({ "title": title }), "items[0]").unwrap_err();
            assert!(err.message.starts_with("Invalid items[0]: "), "{}", err.message);
        }
        assert!(
            parse_new_item(&json!({ "title": "flag 🏴\u{E0067}\u{E0062}\u{E007F} ok" }), "i")
                .is_ok()
        );
    }

    #[test]
    fn checks_dates_estimates_and_enums() {
        assert!(parse_new_item(&json!({ "title": "x", "dueAt": "2026-02-30" }), "i").is_err());
        assert!(parse_new_item(&json!({ "title": "x", "dueAt": "2026-02-28" }), "i").is_ok());
        assert!(parse_new_item(&json!({ "title": "x", "estimate": -1 }), "i").is_err());
        assert!(parse_new_item(&json!({ "title": "x", "priority": "asap" }), "i").is_err());
    }

    #[test]
    fn patches_tell_clearing_from_leaving_alone() {
        let patch = parse_patch(&json!({ "assignee": null, "estimate": 3 })).unwrap();
        assert_eq!(patch.assignee, Some(None));
        assert_eq!(patch.estimate, Some(Some(3.0)));
        assert_eq!(patch.parent, None);
        assert!(parse_patch(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn patches_refuse_unknown_fields_and_null_titles() {
        assert!(
            parse_patch(&json!({ "status": "Done" }))
                .unwrap_err()
                .message
                .contains("unknown field")
        );
        assert!(parse_patch(&json!({ "title": null })).is_ok_and(|p| p.title.is_none()));
        assert!(parse_patch(&json!("title")).is_err());
    }
}
