//! Stale policy: declarative rules that flag, comment on or close backlog cards nobody has
//! touched for a while (ADR 0010). The rules are data, read from `.rodu/tidy.json`; the sweep
//! itself is `RoduService::tidy`.

use serde::{Deserialize, Serialize};

use crate::error::{Result, RoduError};
use crate::model::Item;

/// Where a workspace keeps its tidy rules, inside `.rodu/`.
pub const RULES_FILE: &str = "tidy.json";

/// What a rule does to a stale card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TidyAction {
    /// Lists the card and changes nothing.
    Flag,
    /// Adds one comment, once, saying the card is stale.
    Comment,
    /// Adds a comment and moves the card to Canceled.
    Close,
}

/// One stale rule as written in the rules file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TidyRule {
    /// Short name; the rule's changes are recorded as made via `<you>-tidy-<name>`.
    pub name: String,
    /// The collection it sweeps; every collection when left out.
    #[serde(default)]
    pub collection: Option<String>,
    /// How long a card has had no activity before it is stale: `30d`.
    pub stale_after: String,
    /// JQL for more cards to leave alone, such as `priority = urgent`.
    #[serde(default)]
    pub exempt: Option<String>,
    pub action: TidyAction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    rules: Vec<TidyRule>,
}

/// Reads and checks a rules file's text: names unique and simple, a stale age of 1 to 3650 days.
pub fn parse_rules(text: &str) -> Result<Vec<TidyRule>> {
    let file: RulesFile = serde_json::from_str(text).map_err(|e| {
        RoduError::invalid(format!("The tidy rules are not valid: {e}")).with_hint(EXAMPLE_HINT)
    })?;
    if file.rules.is_empty() {
        return Err(RoduError::invalid("The tidy rules file has no rules").with_hint(EXAMPLE_HINT));
    }
    for (i, rule) in file.rules.iter().enumerate() {
        let simple = (1..=20).contains(&rule.name.len())
            && rule.name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && rule.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !simple {
            return Err(RoduError::invalid(format!(
                "Rule name \"{}\" must be 1-20 lowercase letters, digits or dashes",
                rule.name
            )));
        }
        if file.rules[..i].iter().any(|r| r.name == rule.name) {
            return Err(RoduError::invalid(format!("Two rules are named \"{}\"", rule.name)));
        }
        stale_days(rule)?;
    }
    Ok(file.rules)
}

/// The rule's stale age in days.
pub fn stale_days(rule: &TidyRule) -> Result<u32> {
    rule.stale_after
        .strip_suffix('d')
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| (1..=3650).contains(n))
        .ok_or_else(|| {
            RoduError::invalid(format!(
                "Rule \"{}\": stale_after must be days from 1d to 3650d, such as 30d",
                rule.name
            ))
        })
}

pub const EXAMPLE_HINT: &str = r#"Write .rodu/tidy.json, e.g. {"rules": [{"name": "stale", "stale_after": "30d", "action": "flag"}]}"#;

/// What one rule found, and did when applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TidyReport {
    pub rule: String,
    pub action: TidyAction,
    pub days: u32,
    /// The stale cards, least recently active first, each with its last activity.
    pub cards: Vec<(Item, String)>,
    /// Stale cards this rule already commented on since their last activity: left alone.
    pub already: Vec<Item>,
    /// Cards waiting in Triage (and not snoozed) for longer than the stale age.
    pub stuck_in_triage: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rules_and_refuses_bad_ones() {
        let rules = parse_rules(
            r#"{"rules": [{"name": "stale", "collection": "DEMO", "stale_after": "30d",
                "exempt": "priority = urgent", "action": "close"}]}"#,
        )
        .unwrap();
        assert_eq!((rules[0].action, stale_days(&rules[0]).unwrap()), (TidyAction::Close, 30));
        for bad in [
            r#"{"rules": []}"#,
            r#"{"rules": [{"name": "Stale!", "stale_after": "30d", "action": "flag"}]}"#,
            r#"{"rules": [{"name": "a", "stale_after": "30", "action": "flag"}]}"#,
            r#"{"rules": [{"name": "a", "stale_after": "0d", "action": "flag"}]}"#,
            r#"{"rules": [{"name": "a", "stale_after": "9d", "action": "snooze"}]}"#,
            r#"{"rules": [{"name": "a", "stale_after": "9d", "action": "flag", "other": 1}]}"#,
            r#"{"rules": [{"name": "a", "stale_after": "9d", "action": "flag"},
                          {"name": "a", "stale_after": "8d", "action": "flag"}]}"#,
        ] {
            assert!(parse_rules(bad).is_err(), "{bad}");
        }
    }
}
