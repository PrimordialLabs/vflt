//! Domain types shared by every store and the CLI.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// identifiers

/// Item identifier: `vf-<lowercase ulid>`, or `vf-<n>` in a collective that
/// uses sequential ids.
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

    pub fn seq(n: u64) -> Self {
        ItemId(format!("vf-{n}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The number of a sequential id (`vf-42` -> 42).
    pub fn seq_number(&self) -> Option<u64> {
        self.0.strip_prefix("vf-").and_then(|n| n.parse().ok())
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

/// Actor identifier: an agent id (its name), a profile name, or a human's
/// username.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// Filed but still being written: never claimable, hidden from default
    /// lists, and its spec may still change. `promote` releases it.
    Draft,
    Pending,
    Claimed,
    InProgress,
    Blocked,
    NeedsHuman,
    Done,
    Cancelled,
}

impl Status {
    pub const ALL: [Status; 8] = [
        Status::Draft,
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
            Status::Draft => "draft",
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

    /// Open and visible on the working board: neither terminal nor a draft.
    pub fn is_live(&self) -> bool {
        !self.is_terminal() && *self != Status::Draft
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

/// What a raised item needs from a human.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RaiseKind {
    /// A call only a human may make: a policy, a number, a yes or no.
    #[default]
    Decision,
    /// Something only a human can do: credentials, a restart, eyes on a screen.
    Action,
}

impl RaiseKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RaiseKind::Decision => "decision",
            RaiseKind::Action => "action",
        }
    }
}

impl FromStr for RaiseKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "decision" => Ok(RaiseKind::Decision),
            "action" => Ok(RaiseKind::Action),
            other => Err(format!("unknown raise kind `{other}` (decision | action)")),
        }
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

/// Where an item was before it was last handed forward. A bounce returns it
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub stage: Stage,
    #[serde(
        default,
        deserialize_with = "de_opt_actor",
        skip_serializing_if = "Option::is_none"
    )]
    pub assignee: Option<ActorId>,
}

/// Deserialize an optional actor where an empty string means `None`.
fn de_opt_actor<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<ActorId>, D::Error> {
    let s: Option<String> = Option::deserialize(d)?;
    Ok(s.filter(|s| !s.trim().is_empty()).map(ActorId))
}

/// Mutable workflow state. Persisted as `items/<id>/state.toml` by the file
/// store and as a JSON column by the sqlite store.
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
    /// When the item reached done or cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalRef>,
    /// Free-form reason for the current blocked / needs_human status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    /// Set while needs_human: what kind of answer the raise wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raise_kind: Option<RaiseKind>,
    /// The item this one follows up on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ItemId>,
    /// Where a bounce sends the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// How many times the item has been bounced back.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bounces: u32,
    /// Came back by a bounce and nobody has picked it up since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub returned: bool,
    /// A plain-language summary for people scanning the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Deduplication key of a flag: at most one open item carries a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// The item as the store hands it out: state plus the spec and the current
/// claim, if any. Listings leave `spec` empty; `get_item` fills it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    #[serde(flatten)]
    pub state: ItemState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
    /// Short handle; derived from the title when absent.
    pub slug: Option<String>,
    pub summary: Option<String>,
    pub stage: Option<Stage>,
    pub priority: Option<i32>,
    pub owner: ActorId,
    pub assignee: Option<ActorId>,
    pub tags: Vec<String>,
    pub deps: Vec<ItemId>,
    pub external: Option<ExternalRef>,
    pub parent: Option<ItemId>,
    pub key: Option<String>,
    /// File as a draft instead of pending.
    pub draft: bool,
}

/// Metadata changes that are not workflow transitions.
#[derive(Debug, Clone, Default)]
pub struct ItemUpdate {
    pub title: Option<String>,
    /// `Some("")` clears the summary.
    pub summary: Option<String>,
    pub priority: Option<i32>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
    pub add_deps: Vec<ItemId>,
    pub remove_deps: Vec<ItemId>,
    /// `Some(None)` clears the parent.
    pub parent: Option<Option<ItemId>>,
    /// Replace the spec. Allowed only while the item is a draft.
    pub spec: Option<String>,
}

impl ItemUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.summary.is_none()
            && self.priority.is_none()
            && self.add_tags.is_empty()
            && self.remove_tags.is_empty()
            && self.add_deps.is_empty()
            && self.remove_deps.is_empty()
            && self.parent.is_none()
            && self.spec.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFilter {
    pub stage: Option<Stage>,
    /// Any of these statuses; empty means any status.
    pub statuses: Vec<Status>,
    pub assignee: Option<ActorId>,
    pub owner: Option<ActorId>,
    /// Any of these tags; empty means no tag filter.
    pub tags: Vec<String>,
    pub key: Option<String>,
    pub parent: Option<ItemId>,
    /// Only items not updated since this time.
    pub updated_before: Option<DateTime<Utc>>,
}

impl ItemFilter {
    pub fn matches(&self, item: &ItemState) -> bool {
        self.stage.as_ref().is_none_or(|s| &item.stage == s)
            && (self.statuses.is_empty() || self.statuses.contains(&item.status))
            && self
                .assignee
                .as_ref()
                .is_none_or(|a| item.assignee.as_ref() == Some(a))
            && self.owner.as_ref().is_none_or(|o| &item.owner == o)
            && (self.tags.is_empty() || item.tags.iter().any(|t| self.tags.contains(t)))
            && self
                .key
                .as_ref()
                .is_none_or(|k| item.key.as_ref() == Some(k))
            && self
                .parent
                .as_ref()
                .is_none_or(|p| item.parent.as_ref() == Some(p))
            && self.updated_before.is_none_or(|t| item.updated < t)
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
        kind: RaiseKind,
    },
    Handoff {
        to: ActorId,
        stage: Option<Stage>,
    },
    /// Back to pending until every listed item is done or cancelled.
    Wait {
        on: Vec<ItemId>,
    },
    Reopen {
        to: Option<ActorId>,
    },
    Promote,
    Demote,
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
            Transition::Wait { .. } => "wait",
            Transition::Reopen { .. } => "reopen",
            Transition::Promote => "promote",
            Transition::Demote => "demote",
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

/// What an operator wants the agent to do. Written by people and panels,
/// read by the running agent between items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Desired {
    #[default]
    Running,
    /// Take no new work; stay up and resume when set back to running.
    Paused,
    /// Finish the current turn, then exit.
    Stopped,
}

impl Desired {
    pub fn as_str(&self) -> &'static str {
        match self {
            Desired::Running => "running",
            Desired::Paused => "paused",
            Desired::Stopped => "stopped",
        }
    }
}

impl FromStr for Desired {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "running" => Ok(Desired::Running),
            "paused" => Ok(Desired::Paused),
            "stopped" => Ok(Desired::Stopped),
            other => Err(format!("unknown desired state `{other}`")),
        }
    }
}

/// What the agent is actually doing, as it last reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Running a turn.
    Active,
    /// Up, with nothing to do.
    Idle,
    /// Up, honouring a pause.
    Paused,
    /// Up, but a gate hook or spend cap is holding it back.
    Gated,
    #[default]
    Offline,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Active => "active",
            AgentStatus::Idle => "idle",
            AgentStatus::Paused => "paused",
            AgentStatus::Gated => "gated",
            AgentStatus::Offline => "offline",
        }
    }
}

/// The tracked part of an agent: identity plus the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: ActorId,
    pub name: String,
    pub profiles: Vec<String>,
    pub kind: AgentKind,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub registered: DateTime<Utc>,
    /// Collective this agent has been lent to (path or url), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lent_to: Option<String>,
    /// Collective this agent came from, when it is a lent agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lent_from: Option<String>,
    #[serde(default)]
    pub desired: Desired,
    /// Only work routed here by others: no cycle turns, no self-filed items.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold: bool,
    /// Model override for the runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Cycle interval override ("2h"); empty string disables cycles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<String>,
    /// Wall-clock budget override per turn ("45m").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock: Option<String>,
    /// Spend cap per UTC day, in USD, across this agent's runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
    /// One-shot request for a cycle turn now, cleared when it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_requested: Option<DateTime<Utc>>,
}

/// The untracked part of an agent, written by the running agent itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRuntime {
    #[serde(default)]
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Items in the current turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current: Vec<ItemId>,
    /// One line about the last thing the agent did, or why it is gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cycle: Option<DateTime<Utc>>,
    /// Caught up: no cycle turns until work is routed here again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed: Option<DateTime<Utc>>,
}

/// An agent as the store hands it out: config, runtime and liveness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    #[serde(flatten)]
    pub config: AgentConfig,
    #[serde(flatten)]
    pub runtime: AgentRuntime,
    /// Last heartbeat, if the agent has ever run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

impl Agent {
    pub fn id(&self) -> &ActorId {
        &self.config.id
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewAgent {
    pub name: String,
    pub profiles: Vec<String>,
    pub kind: Option<AgentKind>,
    pub host: Option<String>,
    pub remote: Option<String>,
    pub lent_from: Option<String>,
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
    pub actor: Option<ActorId>,
    /// Keep only the most recent N matches.
    pub limit: Option<usize>,
}

impl EventFilter {
    pub fn matches(&self, e: &Event) -> bool {
        self.item
            .as_ref()
            .is_none_or(|i| e.item.as_ref() == Some(i))
            && self.since.is_none_or(|s| e.ts >= s)
            && self.kind.as_ref().is_none_or(|k| &e.kind == k)
            && self.actor.as_ref().is_none_or(|a| &e.actor == a)
    }
}

// ---------------------------------------------------------------------------
// bundles: the portable form of a board, for import and export

/// One line of a bundle file (JSON lines).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BundleRecord {
    Item {
        #[serde(flatten)]
        state: ItemState,
        spec: String,
    },
    Note {
        item: ItemId,
        #[serde(flatten)]
        note: Note,
    },
    Event(Event),
    Agent(AgentConfig),
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

/// A caller-supplied slug, tidied; falls back to the title.
pub fn item_slug(explicit: Option<&str>, title: &str) -> String {
    match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .take(96)
            .collect::<String>()
            .to_lowercase(),
        None => slugify(title),
    }
}

/// Normalise a tag list: trimmed, lowercase, deduplicated, order kept.
pub fn normalize_tags(tags: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
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
        assert_eq!(ItemId::seq(42).as_str(), "vf-42");
        assert_eq!(ItemId::seq(42).seq_number(), Some(42));
        assert_eq!(id.seq_number(), None);
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
        assert_eq!(st.bounces, 0);
        assert!(!st.returned);
        assert!(st.origin.is_none());
    }

    #[test]
    fn status_round_trips() {
        for s in Status::ALL {
            assert_eq!(s.as_str().parse::<Status>().unwrap(), s);
        }
        assert!(!Status::Draft.is_live());
        assert!(Status::Pending.is_live());
        assert!(!Status::Done.is_live());
    }

    #[test]
    fn tags_normalise() {
        assert_eq!(
            normalize_tags(vec![" A ".into(), "a".into(), "".into(), "b".into()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn bundle_records_round_trip() {
        let n = BundleRecord::Note {
            item: ItemId::seq(3),
            note: Note {
                seq: 1,
                actor: ActorId::from("x"),
                at: Utc::now(),
                body: "hi".into(),
            },
        };
        let line = serde_json::to_string(&n).unwrap();
        assert!(line.contains("\"type\":\"note\""));
        let back: BundleRecord = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, BundleRecord::Note { .. }));
    }
}
