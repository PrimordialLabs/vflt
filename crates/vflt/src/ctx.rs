//! Resolved command context: the open store, who is acting, output mode.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use vflt_core::{discover, ActorId, Item, ItemId, Store};

use crate::identity;

pub struct Ctx {
    pub root: PathBuf,
    pub store: Box<dyn Store>,
    pub actor: ActorId,
    pub json: bool,
}

impl Ctx {
    pub fn open(collective: Option<&PathBuf>, actor: Option<&str>, json: bool) -> Result<Self> {
        let cwd = std::env::current_dir().context("current dir")?;
        let root = discover::find(collective.map(|p| p.as_path()), &cwd)?;
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let store = vflt_core::open_store(&root)?;
        Ok(Ctx {
            root,
            store,
            actor: resolve_actor(actor),
            json,
        })
    }

    pub fn store(&self) -> &dyn Store {
        self.store.as_ref()
    }

    /// Exact id, a bare number for a sequential id, or a unique prefix.
    pub fn resolve_id(&self, id: &str) -> Result<ItemId> {
        let id = id.trim();
        let exact = ItemId::from(id);
        if self.store.get_item(&exact).is_ok() {
            return Ok(exact);
        }
        let needle = if id.starts_with("vf-") {
            id.to_string()
        } else {
            format!("vf-{id}")
        };
        let as_seq = ItemId::from(needle.as_str());
        if self.store.get_item(&as_seq).is_ok() {
            return Ok(as_seq);
        }
        let all = self.store.list_items(&Default::default())?;
        let matches: Vec<&Item> = all
            .iter()
            .filter(|i| i.id().as_str().starts_with(&needle))
            .collect();
        match matches.len() {
            1 => Ok(matches[0].id().clone()),
            0 => Err(anyhow!("item not found: {id}")),
            n => Err(anyhow!("{id} is ambiguous: matches {n} items")),
        }
    }

    pub fn resolve_ids(&self, ids: &[String]) -> Result<Vec<ItemId>> {
        ids.iter().map(|i| self.resolve_id(i)).collect()
    }

    pub fn emit<T: Serialize>(&self, value: &T, human: impl FnOnce() -> String) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(value)?);
        } else {
            let s = human();
            if !s.is_empty() {
                print!("{s}");
                if !s.ends_with('\n') {
                    println!();
                }
            }
        }
        Ok(())
    }
}

pub fn resolve_actor(explicit: Option<&str>) -> ActorId {
    if let Some(a) = explicit.map(str::trim).filter(|a| !a.is_empty()) {
        return ActorId::new(a);
    }
    if let Ok(a) = std::env::var("VFLT_AGENT") {
        if !a.trim().is_empty() {
            return ActorId::new(a.trim());
        }
    }
    ActorId::new(identity::username())
}
