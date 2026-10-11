//! The `Store` trait: the source of truth behind a collective.

use std::path::{Path, PathBuf};

use crate::collective::{Collective, StoreKind};
use crate::error::{Error, Result};
use crate::profile::Profile;
use crate::types::*;

pub trait Store: Send + Sync {
    /// The directory holding `collective.toml`.
    fn root(&self) -> &Path;
    fn collective(&self) -> Result<Collective>;
    /// Machine-local scratch: per-item run logs, prompt files, workspaces.
    fn work_dir(&self) -> PathBuf;
    fn profiles_dir(&self) -> PathBuf;

    // items
    fn create_item(&self, spec: &str, meta: NewItem) -> Result<Item>;
    /// The item with its spec and claim.
    fn get_item(&self, id: &ItemId) -> Result<Item>;
    /// Matching items with their claims, sorted by priority then age. Specs
    /// are not loaded; use `get_item` for one.
    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>>;
    /// Atomic: exactly one caller wins a claim on a pending item.
    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;
    /// Release a claim held by `by`, or break a stale one. Returns the item
    /// to `pending` with the assignee it had before the claim.
    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()>;
    /// Unconditionally remove the claim (`release --force`). Recorded as a
    /// `claim.broken` event.
    fn break_claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;
    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item>;
    /// Metadata edits: title, priority, tags, deps, parent, and the spec of a
    /// draft.
    fn update_item(&self, id: &ItemId, by: &ActorId, update: ItemUpdate) -> Result<Item>;
    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note>;
    fn notes(&self, id: &ItemId) -> Result<Vec<Note>>;
    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item>;
    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item>;

    // agents
    fn register_agent(&self, a: NewAgent) -> Result<Agent>;
    fn get_agent(&self, id: &ActorId) -> Result<Agent>;
    /// Write the tracked part of an agent (identity and control plane).
    fn update_agent(&self, cfg: &AgentConfig, by: &ActorId, summary: &str) -> Result<()>;
    /// Write the untracked runtime part, as the running agent sees itself.
    fn set_runtime(&self, id: &ActorId, rt: &AgentRuntime) -> Result<()>;
    fn heartbeat(&self, id: &ActorId) -> Result<()>;
    /// Time of the last heartbeat, if the agent has ever heartbeated.
    fn last_heartbeat(&self, id: &ActorId) -> Result<Option<chrono::DateTime<chrono::Utc>>>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn deregister_agent(&self, id: &ActorId) -> Result<()>;

    // profiles & events
    fn profiles(&self) -> Result<Vec<Profile>> {
        Profile::load_all(&self.profiles_dir(), self.root())
    }
    fn profile(&self, name: &str) -> Result<Profile> {
        Profile::load(&self.profiles_dir(), name, self.root())
    }
    fn append_event(&self, e: Event) -> Result<()>;
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;

    // bundles
    /// Write one bundle record as-is, keeping ids and timestamps. Items must
    /// arrive before their notes and events.
    fn import_record(&self, rec: BundleRecord) -> Result<()>;
    /// Every record of the board, items first.
    fn export_records(&self) -> Result<Vec<BundleRecord>>;
    /// Rewrite the plain-file mirror of every item, for stores that keep one.
    fn rebuild_mirror(&self) -> Result<usize> {
        Err(Error::invalid(
            "mirror",
            "this store keeps no mirror (set store.mirror on a sqlite collective)",
        ))
    }

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

    /// The open item carrying flag key `key`, if any.
    fn open_by_key(&self, key: &str) -> Result<Option<Item>> {
        let items = self.list_items(&ItemFilter {
            key: Some(key.to_string()),
            ..Default::default()
        })?;
        Ok(items.into_iter().find(|i| !i.status.is_terminal()))
    }
}

/// Open the store configured by `<root>/collective.toml`.
pub fn open_store(root: &Path) -> Result<Box<dyn Store>> {
    let coll = Collective::load(&root.join(crate::discover::CONFIG_FILE))?;
    match coll.store.kind {
        StoreKind::File => Ok(Box::new(crate::file_store::FileStore::open(root)?)),
        StoreKind::Sqlite => Ok(Box::new(crate::sqlite_store::SqliteStore::open(root)?)),
        other => Err(Error::StoreNotImplemented(other.as_str().to_string())),
    }
}

/// Create a collective at `root` with the store its config names.
pub fn init_store(root: &Path, coll: Collective) -> Result<Box<dyn Store>> {
    match coll.store.kind {
        StoreKind::File => Ok(Box::new(crate::file_store::FileStore::init(root, coll)?)),
        StoreKind::Sqlite => Ok(Box::new(crate::sqlite_store::SqliteStore::init(
            root, coll,
        )?)),
        other => Err(Error::StoreNotImplemented(other.as_str().to_string())),
    }
}

/// Write the shared parts of a fresh collective: config, default profiles,
/// `.gitignore`. Both stores call this from `init`.
pub(crate) fn write_skeleton(
    root: &Path,
    data: &Path,
    coll: &Collective,
    gitignore: &str,
) -> Result<()> {
    let cfg = root.join(crate::discover::CONFIG_FILE);
    if cfg.exists() {
        return Err(Error::CollectiveExists(root.to_path_buf()));
    }
    std::fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;
    for d in ["profiles", "work"] {
        let p = data.join(d);
        std::fs::create_dir_all(&p).map_err(|e| Error::io(&p, e))?;
    }
    coll.save(&cfg)?;
    for p in crate::defaults::DEFAULT_PROFILES {
        for (ext, text) in [("toml", p.toml), ("md", p.prompt)] {
            let path = data.join("profiles").join(format!("{}.{ext}", p.name));
            std::fs::write(&path, text).map_err(|e| Error::io(&path, e))?;
        }
    }
    merge_gitignore(&root.join(".gitignore"), gitignore)
}

/// Add the lines of `wanted` that `path` lacks, keeping whatever is there:
/// a collective may be created inside a directory that already ignores
/// things of its own.
pub(crate) fn merge_gitignore(path: &Path, wanted: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::io(path, e)),
    };
    let have: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&str> = wanted
        .lines()
        .filter(|l| !l.trim().is_empty() && !have.contains(l.trim()))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut out = existing.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for l in missing {
        out.push_str(l);
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    #[test]
    fn gitignore_is_merged_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".gitignore");
        std::fs::write(&p, "own/\nwork/").unwrap();
        super::merge_gitignore(&p, "# vflt\nwork/\n*.db\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "own/\nwork/\n# vflt\n*.db\n"
        );
        super::merge_gitignore(&p, "*.db\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "own/\nwork/\n# vflt\n*.db\n"
        );
    }
}
