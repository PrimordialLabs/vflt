//! The `Store` trait: the source of truth behind a collective.

use std::path::Path;

use crate::collective::{Collective, StoreKind};
use crate::error::{Error, Result};
use crate::profile::Profile;
use crate::types::*;

pub trait Store: Send + Sync {
    fn collective(&self) -> Result<Collective>;

    // items
    fn create_item(&self, spec: &str, meta: NewItem) -> Result<Item>;
    fn get_item(&self, id: &ItemId) -> Result<Item>;
    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>>;
    /// Atomic: exactly one caller wins a claim on a pending item.
    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;
    /// Release a claim held by `by`, or break a stale claim. Returns the item
    /// to `pending`.
    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()>;
    /// Unconditionally remove the claim (`release --force`). Recorded as a
    /// `claim.broken` event.
    fn break_claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;
    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item>;
    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note>;
    fn notes(&self, id: &ItemId) -> Result<Vec<Note>>;
    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item>;
    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item>;

    // agents
    fn register_agent(&self, a: NewAgent) -> Result<Agent>;
    fn get_agent(&self, id: &ActorId) -> Result<Agent>;
    fn update_agent(&self, a: &Agent) -> Result<()>;
    fn heartbeat(&self, id: &ActorId) -> Result<()>;
    /// Time of the last heartbeat, if the agent has ever heartbeated.
    fn last_heartbeat(&self, id: &ActorId) -> Result<Option<chrono::DateTime<chrono::Utc>>>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn deregister_agent(&self, id: &ActorId) -> Result<()>;

    // profiles & events
    fn profiles(&self) -> Result<Vec<Profile>>;
    fn profile(&self, name: &str) -> Result<Profile> {
        self.profiles()?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::ProfileNotFound(name.to_string()))
    }
    fn append_event(&self, e: Event) -> Result<()>;
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;

    /// Whether a claim is stale: the holder is a registered agent whose last
    /// heartbeat is older than the collective's `claim_ttl`. Human claims never
    /// go stale on their own.
    fn is_stale(&self, claim: &Claim) -> Result<bool> {
        let coll = self.collective()?;
        let ttl = chrono::Duration::from_std(coll.claim_ttl())
            .map_err(|e| Error::invalid("claim_ttl", e.to_string()))?;
        if self.get_agent(&claim.actor).is_err() {
            return Ok(false);
        }
        let last = self.last_heartbeat(&claim.actor)?.unwrap_or(claim.at);
        let last = last.max(claim.at);
        Ok(chrono::Utc::now() - last > ttl)
    }
}

/// Open the store configured by `<root>/collective.toml`.
pub fn open_store(root: &Path) -> Result<Box<dyn Store>> {
    let coll = Collective::load(&root.join(crate::discover::CONFIG_FILE))?;
    match coll.store.kind {
        StoreKind::File => Ok(Box::new(crate::file_store::FileStore::open(root)?)),
        other => Err(Error::StoreNotImplemented(other.as_str().to_string())),
    }
}
