//! The JSON contract of the local web API, shared by the server (`rodu-http`) and the board
//! (`rodu-web`). It depends on serde only, so it compiles to WebAssembly too.

use serde::{Deserialize, Serialize};

pub const PRIORITIES: &[&str] = &["urgent", "high", "normal", "low", "none"];
pub const ITEM_TYPES: &[&str] = &["task", "bug", "story", "epic", "subtask"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemView {
    pub key: String,
    pub title: String,
    pub body: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub status: String,
    pub category: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub estimate: Option<f64>,
    pub due: Option<String>,
    pub rank: String,
    pub version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateView {
    pub name: String,
    pub category: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionView {
    pub key: String,
    pub name: String,
    pub states: Vec<StateView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentView {
    pub id: String,
    pub author: String,
    pub via: Option<String>,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardView {
    pub collection: CollectionView,
    pub items: Vec<ItemView>,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: ItemView,
    pub comments: Vec<CommentView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalView {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeView {
    pub name: Option<String>,
}

/// Every failure: domain errors and HTTP-level ones alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

/// `POST /api/items`: create one item, optionally straight into a column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub collection: String,
    pub item: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `PATCH /api/items/{key}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PatchRequest {
    pub patch: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<i64>,
}

/// Where to drop a card: between `after` (above) and `before` (below).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
}

/// `POST /api/items/{key}/transition`: a status change, optionally with a new position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionRequest {
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentRequest {
    pub body: String,
}
