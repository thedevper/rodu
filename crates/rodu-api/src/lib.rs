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

/// Rises whenever the board may have changed: a teammate's change came in, or a write here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionView {
    pub revision: u64,
}

/// Every failure: domain errors and HTTP-level ones alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

/// `POST /api/collections`: add a collection; its name is the key when none is given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCollectionRequest {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

// --- the team panel (ADR 0004, step 3a) ------------------------------------------------------

/// `GET /api/team`: the team this workspace syncs with, as `rodu team members` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamView {
    /// The shared folder, as this machine reaches it.
    pub folder: String,
    pub encrypted: bool,
    /// Whether the team admits each machine (a signed team).
    pub signed: bool,
    /// Whether this machine may admit, invite and remove: the owner's or an admin's.
    pub may_admit: bool,
    pub people: Vec<PersonView>,
    /// Machines writing to the folder that no person on the team claims.
    pub unknown: Vec<MachineView>,
    /// Machines asking to join, not yet admitted.
    pub asking: Vec<AskingView>,
    /// The personal invites this machine made that were not used yet.
    pub invites: Vec<InviteView>,
    /// What the list rests on: signatures, or only what each machine says.
    pub trust: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonView {
    pub name: String,
    pub agents: Vec<String>,
    pub machines: Vec<MachineView>,
    /// Whether removing this person can be offered: a machine not removed yet, and none the
    /// owner's or an admin's.
    pub removable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineView {
    /// The first 8 hex digits of its peer id, or all 16 when another machine shares those.
    pub id: String,
    /// Such as "this machine", "numbers cards", "team owner", "admin", "waiting to be admitted",
    /// "removed".
    pub marks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskingView {
    pub name: String,
    /// The machine's code. Never sent to the board: an admin there types the code the person
    /// reads out, and the server compares it, so the check cannot be skipped by copying.
    #[serde(skip)]
    pub code: String,
    pub machine: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteView {
    pub name: String,
    /// The UTC day it ends, `2026-01-31`.
    pub until: String,
    pub expired: bool,
}

/// `POST /api/team/invites`: a personal invite for `name`, lasting `days` (7 when left out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u64>,
}

/// The invite to send, shown once: it holds the join secret and, on an encrypted team, the team
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreated {
    pub name: String,
    pub code: String,
    pub until: String,
}

/// `POST /api/team/invites/revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    pub name: String,
}

/// `POST /api/team/admit`: admit the machine asking to join as `name` whose code is `code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitRequest {
    pub name: String,
    pub code: String,
}

/// `POST /api/team/remove`: remove `name`'s machines, or only the one `machine` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// A place on this machine a plain board can be shared from: a Google Drive folder (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceView {
    /// What the user sees, such as `Shared drive Engineering (ann@example.com)`.
    pub label: String,
    pub path: String,
    /// A shared drive: every member sees it at the same place.
    pub shared: bool,
}

/// `POST /api/team/share`: share this board with a team, its files in `path` (one of the places).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRequest {
    pub path: String,
}

/// What a team change did, to show the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDone {
    pub message: String,
}
