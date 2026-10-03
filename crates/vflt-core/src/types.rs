//! Domain types shared by every store and the CLI.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// identifiers

/// Item identifier: `vf-<lowercase ulid>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

impl ItemId {
    pub fn generate() -> Self {
        ItemId(format!(
            "vf-{}",
            ulid::Ulid::new().to_string().to_lowercase()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Lightweight shape check, not a ulid validation.
    pub fn is_well_formed(&self) -> bool {
        self.0.starts_with("vf-") && self.0.len() > 3
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ItemId {
    fn from(s: &str) -> Self {
        ItemId(s.to_string())
    }
}

/// Actor identifier: an agent id (its name), or a human's username.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(pub String);

impl ActorId {
    pub fn new(s: impl Into<String>) -> Self {
        ActorId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ActorId {
    fn from(s: &str) -> Self {
        ActorId(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// stage & status

/// Pipeline stage. A free-form string so collectives can define their own
/// pipeline; the defaults are provided as constants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Stage(pub String);

impl Stage {
    pub const SURVEY: &'static str = "survey";
    pub const PLAN: &'static str = "plan";
    pub const CODE: &'static str = "code";
    pub const REVIEW: &'static str = "review";
    pub const TEST: &'static str = "test";
    pub const DEPLOY: &'static str = "deploy";
    pub const DONE: &'static str = "done";

    pub fn new(s: impl Into<String>) -> Self {
        Stage(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_done(&self) -> bool {
        self.0 == Self::DONE
    }

    pub fn default_pipeline() -> Vec<Stage> {
        [
            Self::SURVEY,
            Self::PLAN,
            Self::CODE,
            Self::REVIEW,
            Self::TEST,
            Self::DEPLOY,
            Self::DONE,
        ]
        .iter()
        .map(|s| Stage::new(*s))
        .collect()
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Stage {
    fn from(s: &str) -> Self {
        Stage(s.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Claimed,
    InProgress,
    Blocked,
    NeedsHuman,
    Done,
    Cancelled,
}

impl Status {
    pub const ALL: [Status; 7] = [
        Status::Pending,
        Status::Claimed,
        Status::InProgress,
        Status::Blocked,
        Status::NeedsHuman,
        Status::Done,
        Status::Cancelled,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Claimed => "claimed",
            Status::InProgress => "in_progress",
            Status::Blocked => "blocked",
            Status::NeedsHuman => "needs_human",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Done | Status::Cancelled)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Status::ALL
            .iter()
            .copied()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| format!("unknown status `{s}`"))
    }
}

// ---------------------------------------------------------------------------
// items

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRef {
    pub system: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Deserialize an optional actor where an empty string means `None`.
fn de_opt_actor<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<ActorId>, D::Error> {
    let s: Option<String> = Option::deserialize(d)?;
    Ok(s.filter(|s| !s.trim().is_empty()).map(ActorId))
}

/// Mutable workflow state. Persisted as `items/<id>/state.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub id: ItemId,
    pub slug: String,
    pub title: String,
    pub stage: Stage,
    pub status: Status,
    #[serde(default = "default_priority")]
    pub priority: i32,
    pub owner: ActorId,
    #[serde(
        default,
        deserialize_with = "de_opt_actor",
        skip_serializing_if = "Option::is_none"
    )]
    pub assignee: Option<ActorId>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub deps: Vec<ItemId>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalRef>,
    /// Free-form reason for the current blocked / needs_human status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

pub fn default_priority() -> i32 {
    50
}

/// A claim record. Persisted as `items/<id>/.claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub item: ItemId,
    pub actor: ActorId,
    pub at: DateTime<Utc>,
    /// Assignee before the claim, restored on release.
    #[serde(
        default,
        deserialize_with = "de_opt_actor",
        skip_serializing_if = "Option::is_none"
    )]
    pub prior_assignee: Option<ActorId>,
}

/// The item as the store hands it out: state plus the immutable spec and the
/// current claim, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    #[serde(flatten)]
    pub state: ItemState,
    pub spec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
}

impl Item {
    pub fn id(&self) -> &ItemId {
        &self.state.id
    }
}

impl std::ops::Deref for Item {
    type Target = ItemState;

    fn deref(&self) -> &ItemState {
        &self.state
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub title: String,
    pub stage: Option<Stage>,
    pub priority: Option<i32>,
    pub owner: ActorId,
    pub assignee: Option<ActorId>,
    pub tags: Vec<String>,
    pub deps: Vec<ItemId>,
    pub external: Option<ExternalRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFilter {
    pub stage: Option<Stage>,
    pub status: Option<Status>,
    pub assignee: Option<ActorId>,
    pub owner: Option<ActorId>,
    pub tag: Option<String>,
}

impl ItemFilter {
    pub fn matches(&self, item: &ItemState) -> bool {
        self.stage.as_ref().is_none_or(|s| &item.stage == s)
            && self.status.is_none_or(|s| item.status == s)
            && self
                .assignee
                .as_ref()
                .is_none_or(|a| item.assignee.as_ref() == Some(a))
            && self.owner.as_ref().is_none_or(|o| &item.owner == o)
            && self
                .tag
                .as_ref()
                .is_none_or(|t| item.tags.iter().any(|x| x == t))
    }
}

/// Workflow verbs. `claim` and `release` are separate store methods because
/// they have atomicity requirements the others do not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    Start,
    Complete {
        bounce: bool,
        to_stage: Option<Stage>,
        note: Option<String>,
    },
    Block {
        on: String,
    },
    Raise {
        question: String,
    },
    Handoff {
        to: ActorId,
        stage: Option<Stage>,
    },
    Reopen,
    Cancel,
}

impl Transition {
    pub fn verb(&self) -> &'static str {
        match self {
            Transition::Start => "start",
            Transition::Complete { .. } => "complete",
            Transition::Block { .. } => "block",
            Transition::Raise { .. } => "raise",
            Transition::Handoff { .. } => "handoff",
            Transition::Reopen => "reopen",
            Transition::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub seq: u32,
    pub actor: ActorId,
    pub at: DateTime<Utc>,
    pub body: String,
}

// ---------------------------------------------------------------------------
// agents

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Local,
    Remote,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Active,
    Idle,
    Offline,
}

/// Persisted as `agents/<id>.toml`. The id is the agent's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: ActorId,
    pub name: String,
    pub profiles: Vec<String>,
    pub kind: AgentKind,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub status: AgentStatus,
    pub registered: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Collective this agent has been lent to (path or url), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lent_to: Option<String>,
    /// Collective this agent came from, when it is a lent agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lent_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct NewAgent {
    pub name: String,
    pub profiles: Vec<String>,
    pub kind: Option<AgentKind>,
    pub host: Option<String>,
    pub remote: Option<String>,
    pub lent_from: Option<String>,
    pub pid: Option<u32>,
}

// ---------------------------------------------------------------------------
// events

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub actor: ActorId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemId>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

impl Event {
    pub fn new(
        actor: &ActorId,
        kind: &str,
        item: Option<&ItemId>,
        summary: impl Into<String>,
    ) -> Self {
        Event {
            ts: Utc::now(),
            actor: actor.clone(),
            kind: kind.to_string(),
            item: item.cloned(),
            summary: summary.into(),
            detail: serde_json::Value::Null,
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub item: Option<ItemId>,
    pub since: Option<DateTime<Utc>>,
    pub kind: Option<String>,
}

impl EventFilter {
    pub fn matches(&self, e: &Event) -> bool {
        self.item
            .as_ref()
            .is_none_or(|i| e.item.as_ref() == Some(i))
            && self.since.is_none_or(|s| e.ts >= s)
            && self.kind.as_ref().is_none_or(|k| &e.kind == k)
    }
}

// ---------------------------------------------------------------------------
// helpers

/// Turn a title into a filesystem- and shell-friendly slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    let out: String = out.chars().take(48).collect();
    if out.is_empty() {
        "item".to_string()
    } else {
        out.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_have_prefix_and_are_lowercase() {
        let id = ItemId::generate();
        assert!(id.is_well_formed());
        assert_eq!(id.0, id.0.to_lowercase());
        assert_eq!(id.0.len(), 3 + 26);
    }

    #[test]
    fn slugify_is_tidy() {
        assert_eq!(
            slugify("Write RDS Postgres module!"),
            "write-rds-postgres-module"
        );
        assert_eq!(slugify("   "), "item");
    }

    #[test]
    fn empty_assignee_deserializes_as_none() {
        let s = r#"
id = "vf-x"
slug = "x"
title = "x"
stage = "code"
status = "pending"
owner = "me"
assignee = ""
created = "2026-10-02T21:10:00Z"
updated = "2026-10-02T21:10:00Z"
"#;
        let st: ItemState = toml::from_str(s).unwrap();
        assert_eq!(st.assignee, None);
        assert_eq!(st.priority, 50);
    }

    #[test]
    fn status_round_trips() {
        for s in Status::ALL {
            assert_eq!(s.as_str().parse::<Status>().unwrap(), s);
        }
    }
}
