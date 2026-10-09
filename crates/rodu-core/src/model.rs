use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RoduError;

/// Declares a string enum with its wire names, `as_str`, `FromStr`, `ALL` and serde support.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = RoduError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($name::$variant),)+
                    _ => Err(RoduError::invalid(format!(
                        "\"{s}\" is not a valid {}; use one of: {}",
                        stringify!($name).to_lowercase(),
                        [$($text),+].join(", ")
                    ))),
                }
            }
        }
    };
}

string_enum!(
    /// Every workflow state belongs to one category; reports and the board rely on it.
    Category { Backlog => "backlog", Active => "active", Review => "review", Done => "done" }
);
string_enum!(Priority {
    None => "none", Urgent => "urgent", High => "high", Normal => "normal", Low => "low"
});
string_enum!(ItemType {
    Epic => "epic", Story => "story", Task => "task", Bug => "bug", Subtask => "subtask"
});
string_enum!(LinkKind {
    Blocks => "blocks", Relates => "relates", Duplicates => "duplicates",
    ImplementsPr => "implements_pr"
});
string_enum!(CycleState { Planned => "planned", Active => "active", Closed => "closed" });
string_enum!(PrincipalKind { Human => "human", Agent => "agent" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub name: String,
    pub category: Category,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Rule {
    RequireAssignee,
    RequireEstimate,
    RequireLink { link: LinkKind },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// State name, or "*" for any state.
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub states: Vec<State>,
    pub initial: String,
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub id: String,
    pub kind: PrincipalKind,
    pub name: String,
    /// Agents are always owned by a human principal.
    pub owner_id: Option<String>,
}

/// Who performs a command: the principal, and the agent acting for them, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub principal_id: String,
    pub via_agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub id: String,
    pub key: String,
    pub name: String,
    pub preset: String,
    pub workflow: Workflow,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cycle {
    pub id: String,
    pub collection_id: String,
    pub name: String,
    pub starts_on: Option<String>,
    pub ends_on: Option<String>,
    pub state: CycleState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub collection_id: String,
    /// None until the workspace's numbering peer gives the card a number.
    pub number: Option<i64>,
    /// Human-readable short id: DEMO-12 once numbered, the provisional key before that.
    pub key: String,
    /// The key the card had before it was numbered, e.g. DEMO-KQMRTZ; it keeps resolving.
    pub provisional_key: Option<String>,
    #[serde(rename = "type")]
    pub item_type: ItemType,
    pub title: String,
    /// Markdown body.
    pub body: String,
    pub status: String,
    pub category: Category,
    pub priority: Priority,
    pub assignee_id: Option<String>,
    pub parent_id: Option<String>,
    pub cycle_id: Option<String>,
    pub estimate: Option<f64>,
    pub rank: String,
    pub due_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub item_id: String,
    pub author_id: String,
    pub via_agent_id: Option<String>,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub id: String,
    pub from_item_id: String,
    pub kind: LinkKind,
    /// An item id, or a URL for implements_pr.
    pub target: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub request_id: String,
    pub actor_id: String,
    pub via_agent_id: Option<String>,
    pub action: String,
    pub target_id: String,
    pub before: serde_json::Value,
    pub after: serde_json::Value,
    pub at: String,
}
