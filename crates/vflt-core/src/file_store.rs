//! The git-tracked file store. Layout (relative to the collective root):
//!
//! ```text
//! collective.toml
//! items/<id>/{spec.md,state.toml,notes/NNNN-<actor>.md,events.jsonl,.claim}
//! agents/<id>.toml           identity and control plane, tracked
//! agents/<id>.hb             heartbeat timestamp, ignored by git
//! agents/<id>.rt.json        runtime status, ignored by git
//! profiles/<name>.{toml,md}
//! events.jsonl
//! work/                      per-item workspaces and run logs, ignored by git
//! ```

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::collective::{Collective, IdStyle, StoreKind};
use crate::discover::CONFIG_FILE;
use crate::error::{Error, Result};
use crate::git;
use crate::machine;
use crate::store::{write_skeleton, Store};
use crate::types::*;

pub const GITIGNORE: &str = "# vflt: machine-local liveness state and per-item workspaces are not part of the board\nwork/\nagents/*.hb\nagents/*.rt.json\nitems/*/.claim\n";

pub struct FileStore {
    /// Directory holding `collective.toml`.
    root: PathBuf,
    /// Directory holding items/, agents/, ... (`store.root`, usually `.`).
    data: PathBuf,
    collective: Collective,
    git_repo: Option<PathBuf>,
}

impl FileStore {
    /// Create a new collective at `root`.
    pub fn init(root: &Path, mut collective: Collective) -> Result<Self> {
        collective.store.kind = StoreKind::File;
        let data = root.join(&collective.store.root);
        write_skeleton(root, &data, &collective, GITIGNORE)?;
        for d in ["items", "agents"] {
            mkdir(&data.join(d))?;
        }
        touch(&data.join("events.jsonl"))?;

        let git_repo = if collective.store.git {
            if !git::is_available() {
                return Err(Error::Git(
                    "store.git = true but git is not installed; use --no-git".into(),
                ));
            }
            Some(git::ensure_repo(root)?)
        } else {
            None
        };
        let store = FileStore {
            root: root.to_path_buf(),
            data,
            collective,
            git_repo,
        };
        let actor = ActorId::new("vflt");
        // One commit for the whole initial layout: write the event without
        // committing, then commit the root (config, profiles, event log).
        store.append_event_raw(&Event::new(
            &actor,
            "collective.init",
            None,
            format!("initialised collective {}", store.collective.name),
        ))?;
        store.commit(
            std::slice::from_ref(&store.root),
            &format!("vflt: init collective {}", store.collective.name),
        );
        Ok(store)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let cfg = root.join(CONFIG_FILE);
        let collective = Collective::load(&cfg)?;
        if collective.store.kind != StoreKind::File {
            return Err(Error::StoreNotImplemented(
                collective.store.kind.as_str().to_string(),
            ));
        }
        let data = root.join(&collective.store.root);
        let git_repo = if collective.store.git {
            git::toplevel(root)
        } else {
            None
        };
        Ok(FileStore {
            root: root.to_path_buf(),
            data,
            collective,
            git_repo,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    // -- paths --------------------------------------------------------------

    fn item_dir(&self, id: &ItemId) -> PathBuf {
        self.data.join("items").join(id.as_str())
    }

    fn state_path(&self, id: &ItemId) -> PathBuf {
        self.item_dir(id).join("state.toml")
    }

    fn spec_path(&self, id: &ItemId) -> PathBuf {
        self.item_dir(id).join("spec.md")
    }

    fn claim_path(&self, id: &ItemId) -> PathBuf {
        self.item_dir(id).join(".claim")
    }

    fn notes_dir(&self, id: &ItemId) -> PathBuf {
        self.item_dir(id).join("notes")
    }

    fn item_events_path(&self, id: &ItemId) -> PathBuf {
        self.item_dir(id).join("events.jsonl")
    }

    fn events_path(&self) -> PathBuf {
        self.data.join("events.jsonl")
    }

    fn agent_path(&self, id: &ActorId) -> PathBuf {
        self.data
            .join("agents")
            .join(format!("{}.toml", sanitize(id.as_str())))
    }

    fn heartbeat_path(&self, id: &ActorId) -> PathBuf {
        self.data
            .join("agents")
            .join(format!("{}.hb", sanitize(id.as_str())))
    }

    fn runtime_path(&self, id: &ActorId) -> PathBuf {
        self.data
            .join("agents")
            .join(format!("{}.rt.json", sanitize(id.as_str())))
    }

    // -- io helpers ---------------------------------------------------------

    fn read_state(&self, id: &ItemId) -> Result<ItemState> {
        let p = self.state_path(id);
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ItemNotFound(id.to_string()))
            }
            Err(e) => return Err(Error::io(&p, e)),
        };
        Ok(toml::from_str(&text)?)
    }

    fn write_state(&self, st: &ItemState) -> Result<PathBuf> {
        let p = self.state_path(&st.id);
        write_atomic(&p, &toml::to_string_pretty(st)?)?;
        Ok(p)
    }

    fn read_claim(&self, id: &ItemId) -> Result<Option<Claim>> {
        let p = self.claim_path(id);
        match fs::read_to_string(&p) {
            Ok(t) => Ok(Some(toml::from_str(&t)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(&p, e)),
        }
    }

    fn remove_claim(&self, id: &ItemId) -> Result<()> {
        let p = self.claim_path(id);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(&p, e)),
        }
    }

    fn load_item(&self, st: ItemState, with_spec: bool) -> Result<Item> {
        let spec = if with_spec {
            let spec_path = self.spec_path(&st.id);
            fs::read_to_string(&spec_path).map_err(|e| Error::io(&spec_path, e))?
        } else {
            String::new()
        };
        let claim = self.read_claim(&st.id)?;
        Ok(Item {
            state: st,
            spec,
            claim,
        })
    }

    fn item_ids(&self) -> Result<Vec<ItemId>> {
        let dir = self.data.join("items");
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::io(&dir, e)),
        };
        let mut ids: Vec<ItemId> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("state.toml").is_file())
            .map(|e| ItemId(e.file_name().to_string_lossy().to_string()))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Create the directory for a new item and return its id. Sequential ids
    /// race through `create_dir`, which exactly one creator wins.
    fn mint_item_dir(&self) -> Result<ItemId> {
        let items = self.data.join("items");
        mkdir(&items)?;
        match self.collective.ids {
            IdStyle::Ulid => {
                let id = ItemId::generate();
                mkdir(&self.item_dir(&id))?;
                Ok(id)
            }
            IdStyle::Seq => {
                let rd = fs::read_dir(&items).map_err(|e| Error::io(&items, e))?;
                let mut next = rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        ItemId(e.file_name().to_string_lossy().to_string()).seq_number()
                    })
                    .max()
                    .unwrap_or(0)
                    + 1;
                loop {
                    let id = ItemId::seq(next);
                    match fs::create_dir(self.item_dir(&id)) {
                        Ok(()) => return Ok(id),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => next += 1,
                        Err(e) => return Err(Error::io(self.item_dir(&id), e)),
                    }
                }
            }
        }
    }

    fn commit(&self, paths: &[PathBuf], message: &str) {
        if let Some(repo) = &self.git_repo {
            if let Err(e) = git::commit(repo, paths, message) {
                eprintln!("vflt: warning: git commit skipped: {e}");
            }
        }
    }

    /// Append an event to the collective log and, when it names an item, to
    /// that item's log. Returns the paths written.
    fn append_event_raw(&self, e: &Event) -> Result<Vec<PathBuf>> {
        let line = format!("{}\n", serde_json::to_string(e)?);
        let mut paths = vec![self.events_path()];
        append(&self.events_path(), &line)?;
        if let Some(id) = &e.item {
            let p = self.item_events_path(id);
            if p.parent().is_some_and(|d| d.exists()) {
                append(&p, &line)?;
                paths.push(p);
            }
        }
        Ok(paths)
    }

    /// Record an event and commit it together with the paths it touched.
    fn record(&self, e: Event, touched: &[PathBuf]) -> Result<()> {
        let mut paths: Vec<PathBuf> = touched.to_vec();
        paths.extend(self.append_event_raw(&e)?);
        self.commit(&paths, &format!("vflt: {}", e.summary));
        Ok(())
    }

    fn next_note_seq(&self, id: &ItemId) -> Result<u32> {
        Ok(self.notes(id)?.last().map(|n| n.seq + 1).unwrap_or(1))
    }

    fn write_note(&self, id: &ItemId, note: &Note) -> Result<PathBuf> {
        let dir = self.notes_dir(id);
        mkdir(&dir)?;
        let path = dir.join(note_file_name(note));
        write(&path, &format_note(note))?;
        Ok(path)
    }

    fn check_deps(&self, id: Option<&ItemId>, deps: &[ItemId]) -> Result<()> {
        for d in deps {
            if !self.state_path(d).exists() {
                return Err(Error::ItemNotFound(format!("dependency {d}")));
            }
        }
        if let Some(id) = id {
            if machine::creates_cycle(id, deps, |d| Ok(self.read_state(d)?.deps))? {
                return Err(Error::invalid(
                    "deps",
                    format!("depending on {} would create a cycle", join_ids(deps)),
                ));
            }
        }
        Ok(())
    }

    fn read_agent_config(&self, id: &ActorId) -> Result<AgentConfig> {
        let p = self.agent_path(id);
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::AgentNotFound(id.to_string()))
            }
            Err(e) => return Err(Error::io(&p, e)),
        };
        Ok(toml::from_str(&text)?)
    }

    fn read_runtime(&self, id: &ActorId) -> Result<AgentRuntime> {
        let p = self.runtime_path(id);
        match fs::read_to_string(&p) {
            Ok(t) => Ok(serde_json::from_str(&t).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgentRuntime::default()),
            Err(e) => Err(Error::io(&p, e)),
        }
    }

    fn assemble_agent(&self, config: AgentConfig) -> Result<Agent> {
        let runtime = self.read_runtime(&config.id)?;
        let last_seen = self.last_heartbeat(&config.id)?;
        Ok(Agent {
            config,
            runtime,
            last_seen,
        })
    }
}

// ---------------------------------------------------------------------------

impl Store for FileStore {
    fn root(&self) -> &Path {
        &self.root
    }

    fn collective(&self) -> Result<Collective> {
        Ok(self.collective.clone())
    }

    fn work_dir(&self) -> PathBuf {
        self.data.join("work")
    }

    fn profiles_dir(&self) -> PathBuf {
        self.data.join("profiles")
    }

    fn create_item(&self, spec: &str, meta: NewItem) -> Result<Item> {
        if meta.title.trim().is_empty() {
            return Err(Error::invalid("item", "title is empty"));
        }
        self.check_deps(None, &meta.deps)?;
        if let Some(p) = &meta.parent {
            if !self.state_path(p).exists() {
                return Err(Error::ItemNotFound(format!("parent {p}")));
            }
        }
        if let Some(k) = &meta.key {
            if let Some(open) = self.open_by_key(k)? {
                return Err(Error::invalid(
                    "key",
                    format!("{} already carries key {k:?}", open.id()),
                ));
            }
        }
        let now = Utc::now();
        let id = self.mint_item_dir()?;
        let stage = meta.stage.unwrap_or_else(|| {
            self.collective
                .pipeline
                .first()
                .cloned()
                .unwrap_or(Stage::new(Stage::CODE))
        });
        let origin = machine::origin_for_new(&stage, &meta.owner, meta.assignee.as_ref());
        let st = ItemState {
            id: id.clone(),
            slug: item_slug(meta.slug.as_deref(), &meta.title),
            title: meta.title.trim().to_string(),
            stage,
            status: if meta.draft {
                Status::Draft
            } else {
                Status::Pending
            },
            priority: meta.priority.unwrap_or_else(default_priority),
            owner: meta.owner.clone(),
            assignee: meta.assignee,
            tags: normalize_tags(meta.tags),
            deps: meta.deps,
            created: now,
            updated: now,
            closed: None,
            external: meta.external,
            waiting_on: None,
            raise_kind: None,
            parent: meta.parent,
            origin,
            bounces: 0,
            returned: false,
            summary: meta.summary.clone().filter(|s| !s.trim().is_empty()),
            key: meta.key,
        };
        let dir = self.item_dir(&id);
        mkdir(&self.notes_dir(&id))?;
        let spec_text = if spec.trim().is_empty() {
            format!("# {}\n", st.title)
        } else {
            normalize_newlines(spec)
        };
        write(&self.spec_path(&id), &spec_text)?;
        touch(&self.item_events_path(&id))?;
        self.write_state(&st)?;
        self.record(
            Event::new(&meta.owner, "item.created", Some(&id), format!("created {id} \"{}\" at {}", st.title, st.stage))
                .with_detail(serde_json::json!({"stage": st.stage, "status": st.status, "priority": st.priority, "assignee": st.assignee})),
            &[dir],
        )?;
        self.load_item(st, true)
    }

    fn get_item(&self, id: &ItemId) -> Result<Item> {
        let st = self.read_state(id)?;
        self.load_item(st, true)
    }

    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        for id in self.item_ids()? {
            let st = self.read_state(&id)?;
            if filter.matches(&st) {
                out.push(self.load_item(st, false)?);
            }
        }
        out.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.created.cmp(&b.created)));
        Ok(out)
    }

    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim> {
        let st = self.read_state(id)?;
        if st.status != Status::Pending {
            return Err(Error::InvalidTransition {
                item: id.to_string(),
                verb: "claim",
                from: st.status.to_string(),
            });
        }
        let claim = Claim {
            item: id.clone(),
            actor: by.clone(),
            at: Utc::now(),
            prior_assignee: st.assignee.clone(),
        };
        let path = self.claim_path(id);
        // create_new is atomic on POSIX and Windows: exactly one caller wins.
        let mut f = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = self
                    .read_claim(id)?
                    .map(|c| c.actor.to_string())
                    .unwrap_or_else(|| "unknown".into());
                return Err(Error::AlreadyClaimed {
                    item: id.to_string(),
                    holder,
                });
            }
            Err(e) => return Err(Error::io(&path, e)),
        };
        f.write_all(toml::to_string(&claim)?.as_bytes())
            .map_err(|e| Error::io(&path, e))?;
        drop(f);

        let mut st = st;
        st.status = Status::Claimed;
        st.assignee = Some(by.clone());
        st.returned = false;
        st.updated = Utc::now();
        let p = self.write_state(&st)?;
        self.record(
            Event::new(by, "item.claimed", Some(id), format!("{by} claimed {id}")),
            &[p],
        )?;
        Ok(claim)
    }

    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()> {
        let claim = self.read_claim(id)?.ok_or_else(|| Error::NotClaimed {
            item: id.to_string(),
        })?;
        let kind = if &claim.actor == by {
            "item.released"
        } else if self.is_stale(&claim)? {
            "claim.broken"
        } else {
            return Err(Error::NotClaimHolder {
                item: id.to_string(),
                holder: claim.actor.to_string(),
                actor: by.to_string(),
            });
        };
        self.remove_claim(id)?;
        let mut st = self.read_state(id)?;
        if matches!(st.status, Status::Claimed | Status::InProgress) {
            st.status = Status::Pending;
            st.assignee = claim.prior_assignee.clone();
            st.updated = Utc::now();
        }
        let p = self.write_state(&st)?;
        let summary = if kind == "claim.broken" {
            format!("{by} broke stale claim on {id} held by {}", claim.actor)
        } else {
            format!("{by} released {id}")
        };
        self.record(Event::new(by, kind, Some(id), summary), &[p])
    }

    fn break_claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim> {
        let claim = self.read_claim(id)?.ok_or_else(|| Error::NotClaimed {
            item: id.to_string(),
        })?;
        self.remove_claim(id)?;
        let mut st = self.read_state(id)?;
        if matches!(st.status, Status::Claimed | Status::InProgress) {
            st.status = Status::Pending;
            st.assignee = claim.prior_assignee.clone();
            st.updated = Utc::now();
        }
        let p = self.write_state(&st)?;
        self.record(
            Event::new(
                by,
                "claim.broken",
                Some(id),
                format!("{by} forcibly broke claim on {id} held by {}", claim.actor),
            ),
            &[p],
        )?;
        Ok(claim)
    }

    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item> {
        let mut st = self.read_state(id)?;
        if to == Transition::Start && st.status == Status::Pending {
            // implicit claim
            self.claim(id, by)?;
            st = self.read_state(id)?;
        }
        if let Transition::Wait { on } = &to {
            self.check_deps(Some(id), on)?;
        }
        let claim = self.read_claim(id)?;
        let applied = machine::apply(
            &self.collective,
            &mut st,
            claim.as_ref(),
            by,
            to,
            Utc::now(),
        )?;
        if applied.drop_claim {
            self.remove_claim(id)?;
        }
        let mut touched = vec![self.write_state(&st)?];
        if let Some(body) = applied.note {
            let seq = self.next_note_seq(id)?;
            let n = Note {
                seq,
                actor: by.clone(),
                at: Utc::now(),
                body: normalize_newlines(body.trim_end()).trim_end().to_string(),
            };
            touched.push(self.write_note(id, &n)?);
        }
        self.record(
            Event::new(by, applied.kind, Some(id), applied.summary).with_detail(applied.detail),
            &touched,
        )?;
        self.load_item(st, false)
    }

    fn update_item(&self, id: &ItemId, by: &ActorId, u: ItemUpdate) -> Result<Item> {
        let mut st = self.read_state(id)?;
        if u.is_empty() {
            return self.load_item(st, true);
        }
        let mut touched = vec![self.state_path(id)];
        let mut changes: Vec<String> = Vec::new();
        if !u.add_deps.is_empty() {
            self.check_deps(Some(id), &u.add_deps)?;
        }
        if let Some(Some(p)) = &u.parent {
            if !self.state_path(p).exists() {
                return Err(Error::ItemNotFound(format!("parent {p}")));
            }
        }
        if let Some(spec) = &u.spec {
            if st.status != Status::Draft {
                return Err(Error::invalid(
                    "spec",
                    format!(
                        "{id} is {}; only a draft's spec may change (add a note instead)",
                        st.status
                    ),
                ));
            }
            let p = self.spec_path(id);
            write(&p, &normalize_newlines(spec))?;
            touched.push(p);
            changes.push("spec".into());
        }
        apply_update(&mut st, u, &mut changes);
        st.updated = Utc::now();
        self.write_state(&st)?;
        self.record(
            Event::new(
                by,
                "item.updated",
                Some(id),
                format!("{by} updated {id}: {}", changes.join(", ")),
            )
            .with_detail(serde_json::json!({"changes": changes})),
            &touched,
        )?;
        self.get_item(id)
    }

    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note> {
        self.read_state(id)?;
        let seq = self.next_note_seq(id)?;
        let note = Note {
            seq,
            actor: by.clone(),
            at: Utc::now(),
            body: normalize_newlines(body.trim_end()).trim_end().to_string(),
        };
        let path = self.write_note(id, &note)?;
        self.record(
            Event::new(
                by,
                "item.noted",
                Some(id),
                format!("{by} noted {id} (#{seq})"),
            ),
            &[path],
        )?;
        Ok(note)
    }

    fn notes(&self, id: &ItemId) -> Result<Vec<Note>> {
        let dir = self.notes_dir(id);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::io(&dir, e)),
        };
        let mut out = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let text = fs::read_to_string(&p).map_err(|e| Error::io(&p, e))?;
            if let Some(n) = parse_note(&text) {
                out.push(n);
            }
        }
        out.sort_by_key(|n| n.seq);
        Ok(out)
    }

    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item> {
        let mut st = self.read_state(id)?;
        st.assignee = to.clone();
        st.updated = Utc::now();
        let p = self.write_state(&st)?;
        let summary = match &to {
            Some(a) => format!("{by} assigned {id} to {a}"),
            None => format!("{by} returned {id} to the pool"),
        };
        self.record(Event::new(by, "item.assigned", Some(id), summary), &[p])?;
        self.load_item(st, false)
    }

    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item> {
        let mut st = self.read_state(id)?;
        let summary = format!("{by} linked {id} to {}:{}", ext.system, ext.key);
        st.external = Some(ext);
        st.updated = Utc::now();
        let p = self.write_state(&st)?;
        self.record(Event::new(by, "item.linked", Some(id), summary), &[p])?;
        self.load_item(st, false)
    }

    // -- agents -------------------------------------------------------------

    fn register_agent(&self, a: NewAgent) -> Result<Agent> {
        let name = a.name.trim();
        if name.is_empty() {
            return Err(Error::invalid("agent", "name is empty"));
        }
        let id = ActorId::new(name);
        let existing = self.read_agent_config(&id).ok();
        let cfg = new_agent_config(&id, a, existing.as_ref());
        mkdir(&self.data.join("agents"))?;
        let p = self.agent_path(&id);
        write_atomic(&p, &toml::to_string_pretty(&cfg)?)?;
        let kind = if existing.is_some() {
            "agent.resumed"
        } else {
            "agent.registered"
        };
        self.record(
            Event::new(
                &id,
                kind,
                None,
                format!(
                    "agent {id} {} with profiles {:?}",
                    if existing.is_some() {
                        "resumed"
                    } else {
                        "registered"
                    },
                    cfg.profiles
                ),
            ),
            &[p],
        )?;
        self.assemble_agent(cfg)
    }

    fn get_agent(&self, id: &ActorId) -> Result<Agent> {
        let cfg = self.read_agent_config(id)?;
        self.assemble_agent(cfg)
    }

    fn update_agent(&self, cfg: &AgentConfig, by: &ActorId, summary: &str) -> Result<()> {
        if !self.agent_path(&cfg.id).exists() {
            return Err(Error::AgentNotFound(cfg.id.to_string()));
        }
        let p = self.agent_path(&cfg.id);
        write_atomic(&p, &toml::to_string_pretty(cfg)?)?;
        self.record(
            Event::new(by, "agent.updated", None, summary.to_string()),
            &[p],
        )
    }

    fn set_runtime(&self, id: &ActorId, rt: &AgentRuntime) -> Result<()> {
        // Runtime is machine-local liveness; never committed.
        write_atomic(&self.runtime_path(id), &serde_json::to_string_pretty(rt)?)
    }

    fn heartbeat(&self, id: &ActorId) -> Result<()> {
        // Heartbeats are frequent and deliberately not committed to git.
        let p = self.heartbeat_path(id);
        write_atomic(&p, &format!("{}\n", Utc::now().to_rfc3339()))
    }

    fn last_heartbeat(&self, id: &ActorId) -> Result<Option<DateTime<Utc>>> {
        let p = self.heartbeat_path(id);
        match fs::read_to_string(&p) {
            Ok(t) => Ok(DateTime::parse_from_rfc3339(t.trim())
                .ok()
                .map(|d| d.with_timezone(&Utc))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(&p, e)),
        }
    }

    fn list_agents(&self) -> Result<Vec<Agent>> {
        let dir = self.data.join("agents");
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::io(&dir, e)),
        };
        let mut out = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().is_none_or(|x| x != "toml") {
                continue;
            }
            let text = fs::read_to_string(&p).map_err(|e| Error::io(&p, e))?;
            let cfg: AgentConfig = toml::from_str(&text)?;
            out.push(self.assemble_agent(cfg)?);
        }
        out.sort_by(|a, b| a.config.name.cmp(&b.config.name));
        Ok(out)
    }

    fn deregister_agent(&self, id: &ActorId) -> Result<()> {
        let p = self.agent_path(id);
        if !p.exists() {
            return Err(Error::AgentNotFound(id.to_string()));
        }
        fs::remove_file(&p).map_err(|e| Error::io(&p, e))?;
        let _ = fs::remove_file(self.heartbeat_path(id));
        let _ = fs::remove_file(self.runtime_path(id));
        self.record(
            Event::new(
                id,
                "agent.deregistered",
                None,
                format!("agent {id} deregistered"),
            ),
            &[p],
        )
    }

    // -- events -------------------------------------------------------------

    fn append_event(&self, e: Event) -> Result<()> {
        let paths = self.append_event_raw(&e)?;
        self.commit(&paths, &format!("vflt: {}", e.summary));
        Ok(())
    }

    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>> {
        let p = match &filter.item {
            Some(id) => self.item_events_path(id),
            None => self.events_path(),
        };
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::io(&p, e)),
        };
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let e: Event = serde_json::from_str(line)?;
            if filter.matches(&e) {
                out.push(e);
            }
        }
        if let Some(n) = filter.limit {
            let skip = out.len().saturating_sub(n);
            out.drain(..skip);
        }
        Ok(out)
    }

    // -- bundles ------------------------------------------------------------

    fn import_record(&self, rec: BundleRecord) -> Result<()> {
        match rec {
            BundleRecord::Item { state, spec } => {
                let dir = self.item_dir(&state.id);
                if dir.join("state.toml").exists() {
                    return Err(Error::invalid(
                        "import",
                        format!("{} already exists", state.id),
                    ));
                }
                mkdir(&self.notes_dir(&state.id))?;
                write(&self.spec_path(&state.id), &normalize_newlines(&spec))?;
                touch(&self.item_events_path(&state.id))?;
                self.write_state(&state)?;
            }
            BundleRecord::Note { item, note } => {
                self.read_state(&item)?;
                self.write_note(&item, &note)?;
            }
            BundleRecord::Event(e) => {
                self.append_event_raw(&e)?;
            }
            BundleRecord::Agent(cfg) => {
                mkdir(&self.data.join("agents"))?;
                write_atomic(&self.agent_path(&cfg.id), &toml::to_string_pretty(&cfg)?)?;
            }
        }
        Ok(())
    }

    fn export_records(&self) -> Result<Vec<BundleRecord>> {
        let mut out = Vec::new();
        let ids = self.item_ids()?;
        for id in &ids {
            let st = self.read_state(id)?;
            let spec = fs::read_to_string(self.spec_path(id))
                .map_err(|e| Error::io(self.spec_path(id), e))?;
            out.push(BundleRecord::Item { state: st, spec });
        }
        for id in &ids {
            for note in self.notes(id)? {
                out.push(BundleRecord::Note {
                    item: id.clone(),
                    note,
                });
            }
        }
        for e in self.events(&EventFilter::default())? {
            out.push(BundleRecord::Event(e));
        }
        for a in self.list_agents()? {
            out.push(BundleRecord::Agent(a.config));
        }
        Ok(out)
    }
}

/// Apply the metadata half of an update (everything except the spec).
pub(crate) fn apply_update(st: &mut ItemState, u: ItemUpdate, changes: &mut Vec<String>) {
    if let Some(t) = u.title {
        let t = t.trim().to_string();
        if !t.is_empty() && t != st.title {
            st.title = t;
            changes.push("title".into());
        }
    }
    if let Some(sm) = u.summary {
        let sm = sm.trim().to_string();
        let new = if sm.is_empty() { None } else { Some(sm) };
        if new != st.summary {
            st.summary = new;
            changes.push("summary".into());
        }
    }
    if let Some(p) = u.priority {
        if p != st.priority {
            st.priority = p;
            changes.push(format!("priority {p}"));
        }
    }
    let add = normalize_tags(u.add_tags);
    let remove = normalize_tags(u.remove_tags);
    for t in add {
        if !st.tags.contains(&t) {
            changes.push(format!("+{t}"));
            st.tags.push(t);
        }
    }
    for t in remove {
        if let Some(i) = st.tags.iter().position(|x| x == &t) {
            st.tags.remove(i);
            changes.push(format!("-{t}"));
        }
    }
    for d in u.add_deps {
        if !st.deps.contains(&d) && d != st.id {
            changes.push(format!("dep +{d}"));
            st.deps.push(d);
        }
    }
    for d in u.remove_deps {
        if let Some(i) = st.deps.iter().position(|x| x == &d) {
            st.deps.remove(i);
            changes.push(format!("dep -{d}"));
        }
    }
    if let Some(p) = u.parent {
        if p != st.parent {
            changes.push(match &p {
                Some(p) => format!("parent {p}"),
                None => "parent cleared".into(),
            });
            st.parent = p;
        }
    }
}

pub(crate) fn new_agent_config(
    id: &ActorId,
    a: NewAgent,
    existing: Option<&AgentConfig>,
) -> AgentConfig {
    let now = Utc::now();
    match existing {
        Some(e) => {
            let mut c = e.clone();
            if !a.profiles.is_empty() {
                c.profiles = a.profiles;
            }
            if let Some(k) = a.kind {
                c.kind = k;
            }
            if let Some(h) = a.host {
                c.host = h;
            }
            if a.remote.is_some() {
                c.remote = a.remote;
            }
            if a.lent_from.is_some() {
                c.lent_from = a.lent_from;
            }
            c
        }
        None => AgentConfig {
            id: id.clone(),
            name: id.to_string(),
            profiles: a.profiles,
            kind: a.kind.unwrap_or(AgentKind::Local),
            host: a.host.unwrap_or_else(hostname),
            remote: a.remote,
            registered: now,
            lent_to: None,
            lent_from: a.lent_from,
            desired: Desired::Running,
            hold: false,
            model: None,
            cycle: None,
            wall_clock: None,
            daily_usd: None,
            wake_requested: None,
        },
    }
}

pub(crate) fn join_ids(ids: &[ItemId]) -> String {
    ids.iter()
        .map(|d| d.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// notes on disk

fn note_file_name(n: &Note) -> String {
    format!("{:04}-{}.md", n.seq, sanitize(n.actor.as_str()))
}

fn format_note(n: &Note) -> String {
    format!(
        "---\nseq: {}\nactor: {}\nat: {}\n---\n{}\n",
        n.seq,
        n.actor,
        n.at.to_rfc3339(),
        n.body
    )
}

fn parse_note(text: &str) -> Option<Note> {
    let text = text.replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let (head, body) = (&rest[..end], &rest[end + 5..]);
    let mut seq = None;
    let mut actor = None;
    let mut at = None;
    for line in head.lines() {
        if let Some(v) = line.strip_prefix("seq: ") {
            seq = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("actor: ") {
            actor = Some(ActorId::new(v.trim()));
        } else if let Some(v) = line.strip_prefix("at: ") {
            at = DateTime::parse_from_rfc3339(v.trim())
                .ok()
                .map(|d| d.with_timezone(&Utc));
        }
    }
    Some(Note {
        seq: seq?,
        actor: actor?,
        at: at?,
        body: body.trim_end().to_string(),
    })
}

// ---------------------------------------------------------------------------
// fs helpers

fn mkdir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).map_err(|e| Error::io(p, e))
}

fn write(p: &Path, text: &str) -> Result<()> {
    fs::write(p, text.as_bytes()).map_err(|e| Error::io(p, e))
}

fn touch(p: &Path) -> Result<()> {
    if !p.exists() {
        write(p, "")?;
    }
    Ok(())
}

fn append(p: &Path, text: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(p)
        .map_err(|e| Error::io(p, e))?;
    f.write_all(text.as_bytes()).map_err(|e| Error::io(p, e))
}

/// Write via a sibling temp file and rename, so readers never see a torn file.
fn write_atomic(p: &Path, text: &str) -> Result<()> {
    let dir = p
        .parent()
        .ok_or_else(|| Error::invalid("path", p.display().to_string()))?;
    mkdir(dir)?;
    let tmp = dir.join(
        format!(
            ".{}.{}.{:?}.tmp",
            p.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            std::process::id(),
            std::thread::current().id()
        )
        .replace(['(', ')'], ""),
    );
    write(&tmp, text)?;
    fs::rename(&tmp, p).map_err(|e| Error::io(p, e))
}

pub(crate) fn normalize_newlines(s: &str) -> String {
    let mut out = s.replace("\r\n", "\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults;
    use std::sync::Arc;

    fn store() -> (tempfile::TempDir, FileStore) {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collective::new("test");
        c.store.git = false;
        let s = FileStore::init(&tmp.path().join(".vflt"), c).unwrap();
        (tmp, s)
    }

    fn add(s: &FileStore, title: &str, stage: &str) -> Item {
        s.create_item(
            "spec body",
            NewItem {
                title: title.into(),
                stage: Some(Stage::new(stage)),
                owner: ActorId::from("tester"),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn init_writes_layout_and_profiles() {
        let (_t, s) = store();
        assert!(s.root().join("collective.toml").is_file());
        assert!(s.root().join(".gitignore").is_file());
        let profiles = s.profiles().unwrap();
        assert_eq!(profiles.len(), defaults::DEFAULT_PROFILES.len());
        assert!(profiles.iter().any(|p| p.name == "coder"));
        assert!(matches!(
            FileStore::init(s.root(), Collective::new("dup")),
            Err(Error::CollectiveExists(_))
        ));
    }

    #[test]
    fn crud_round_trip() {
        let (_t, s) = store();
        let it = add(&s, "Write the thing", "code");
        assert!(it.id().is_well_formed());
        assert_eq!(it.slug, "write-the-thing");
        assert_eq!(it.status, Status::Pending);
        let got = s.get_item(it.id()).unwrap();
        assert_eq!(got.spec, "spec body\n");
        assert_eq!(got.state, it.state);

        let all = s.list_items(&ItemFilter::default()).unwrap();
        assert_eq!(all.len(), 1);
        let none = s
            .list_items(&ItemFilter {
                stage: Some(Stage::new("review")),
                ..Default::default()
            })
            .unwrap();
        assert!(none.is_empty());

        let by = ActorId::from("tester");
        let n = s.add_note(it.id(), &by, "first note\r\nwith crlf").unwrap();
        assert_eq!(n.seq, 1);
        let notes = s.notes(it.id()).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body, "first note\nwith crlf");

        let linked = s
            .link_external(
                it.id(),
                &by,
                ExternalRef {
                    system: "jira".into(),
                    key: "PAY-1".into(),
                    url: None,
                },
            )
            .unwrap();
        assert_eq!(linked.external.as_ref().unwrap().key, "PAY-1");

        let evs = s
            .events(&EventFilter {
                item: Some(it.id().clone()),
                ..Default::default()
            })
            .unwrap();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["item.created", "item.noted", "item.linked"]);
        assert!(matches!(
            s.get_item(&ItemId::from("vf-nope")),
            Err(Error::ItemNotFound(_))
        ));
    }

    #[test]
    fn claim_race_has_exactly_one_winner() {
        let (_t, s) = store();
        let it = add(&s, "contended", "code");
        let s = Arc::new(s);
        let id = it.id().clone();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let s = Arc::clone(&s);
                let id = id.clone();
                std::thread::spawn(move || {
                    s.claim(&id, &ActorId::new(format!("agent-{i}"))).is_ok()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(wins, 1);
        let got = s.get_item(&id).unwrap();
        assert_eq!(got.status, Status::Claimed);
        assert!(got.claim.is_some());
        assert_eq!(
            got.assignee.as_ref(),
            Some(&got.claim.as_ref().unwrap().actor)
        );
    }

    #[test]
    fn transition_table() {
        let (_t, s) = store();
        let coder = ActorId::from("coder-1");
        let reviewer = ActorId::from("reviewer-1");
        let human = ActorId::from("vito");
        let it = add(&s, "pipeline walk", "code");
        let id = it.id().clone();

        // start from pending is an implicit claim
        let it = s.transition(&id, &coder, Transition::Start).unwrap();
        assert_eq!(it.status, Status::InProgress);
        assert_eq!(it.claim.as_ref().unwrap().actor, coder);
        // someone else cannot complete it
        assert!(matches!(
            s.transition(
                &id,
                &reviewer,
                Transition::Complete {
                    bounce: false,
                    to_stage: None,
                    note: None
                }
            ),
            Err(Error::NotClaimHolder { .. })
        ));
        // complete -> review, pending, unassigned, claim gone
        let it = s
            .transition(
                &id,
                &coder,
                Transition::Complete {
                    bounce: false,
                    to_stage: None,
                    note: Some("done".into()),
                },
            )
            .unwrap();
        assert_eq!(it.stage.as_str(), "review");
        assert_eq!(it.status, Status::Pending);
        assert_eq!(it.assignee, None);
        assert!(it.claim.is_none());
        assert_eq!(s.notes(&id).unwrap().len(), 1);

        // reviewer bounces back to code
        s.claim(&id, &reviewer).unwrap();
        s.transition(&id, &reviewer, Transition::Start).unwrap();
        let it = s
            .transition(
                &id,
                &reviewer,
                Transition::Complete {
                    bounce: true,
                    to_stage: None,
                    note: Some("nit".into()),
                },
            )
            .unwrap();
        assert_eq!(it.stage.as_str(), "code");
        assert_eq!(it.status, Status::Pending);

        // block, reopen
        s.transition(&id, &coder, Transition::Start).unwrap();
        let it = s
            .transition(
                &id,
                &coder,
                Transition::Block {
                    on: "vf-other".into(),
                },
            )
            .unwrap();
        assert_eq!(it.status, Status::Blocked);
        assert_eq!(it.waiting_on.as_deref(), Some("vf-other"));
        assert!(matches!(
            s.transition(&id, &coder, Transition::Start),
            Err(Error::InvalidTransition { .. })
        ));
        let it = s
            .transition(&id, &human, Transition::Reopen { to: None })
            .unwrap();
        assert_eq!(it.status, Status::Pending);
        assert_eq!(it.waiting_on, None);

        // raise, handoff with stage change
        s.transition(&id, &coder, Transition::Start).unwrap();
        let it = s
            .transition(
                &id,
                &coder,
                Transition::Raise {
                    question: "which db?".into(),
                    kind: RaiseKind::Decision,
                },
            )
            .unwrap();
        assert_eq!(it.status, Status::NeedsHuman);
        let it = s
            .transition(
                &id,
                &human,
                Transition::Handoff {
                    to: ActorId::from("planner"),
                    stage: Some(Stage::new("plan")),
                },
            )
            .unwrap();
        assert_eq!(it.status, Status::Pending);
        assert_eq!(it.stage.as_str(), "plan");
        assert_eq!(it.assignee.as_ref().map(|a| a.as_str()), Some("planner"));

        // walk to done via explicit stage
        s.transition(&id, &coder, Transition::Start).unwrap();
        let it = s
            .transition(
                &id,
                &coder,
                Transition::Complete {
                    bounce: false,
                    to_stage: Some(Stage::new("deploy")),
                    note: None,
                },
            )
            .unwrap();
        assert_eq!(it.stage.as_str(), "deploy");
        s.transition(&id, &coder, Transition::Start).unwrap();
        let it = s
            .transition(
                &id,
                &coder,
                Transition::Complete {
                    bounce: false,
                    to_stage: None,
                    note: None,
                },
            )
            .unwrap();
        assert_eq!(it.status, Status::Done);
        assert_eq!(it.stage.as_str(), "done");
        assert!(matches!(
            s.transition(&id, &human, Transition::Cancel),
            Err(Error::InvalidTransition { .. })
        ));

        // cancel a fresh one
        let other = add(&s, "doomed", "code");
        let it = s
            .transition(other.id(), &human, Transition::Cancel)
            .unwrap();
        assert_eq!(it.status, Status::Cancelled);
    }

    #[test]
    fn release_restores_prior_assignee_and_breaks_stale_claims() {
        let (_t, s) = store();
        let a = ActorId::from("agent-a");
        let b = ActorId::from("agent-b");
        let it = s
            .create_item(
                "",
                NewItem {
                    title: "delegated".into(),
                    stage: Some(Stage::new("code")),
                    owner: ActorId::from("sup"),
                    assignee: Some(ActorId::from("agent-a")),
                    ..Default::default()
                },
            )
            .unwrap();
        s.claim(it.id(), &a).unwrap();
        // b cannot release a's live claim
        assert!(matches!(
            s.release(it.id(), &b),
            Err(Error::NotClaimHolder { .. })
        ));
        s.release(it.id(), &a).unwrap();
        let got = s.get_item(it.id()).unwrap();
        assert_eq!(got.status, Status::Pending);
        assert_eq!(got.assignee, Some(a.clone()));

        // register a, claim, then age its heartbeat past the ttl
        s.register_agent(NewAgent {
            name: "agent-a".into(),
            profiles: vec!["coder".into()],
            ..Default::default()
        })
        .unwrap();
        s.heartbeat(&a).unwrap();
        let claim = s.claim(it.id(), &a).unwrap();
        assert!(!s.is_stale(&claim).unwrap());
        let old = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        fs::write(s.heartbeat_path(&a), old).unwrap();
        let mut stale_claim = claim.clone();
        stale_claim.at = Utc::now() - chrono::Duration::hours(2);
        fs::write(
            s.claim_path(it.id()),
            toml::to_string(&stale_claim).unwrap(),
        )
        .unwrap();
        assert!(s.is_stale(&stale_claim).unwrap());
        s.release(it.id(), &b).unwrap();
        let evs = s
            .events(&EventFilter {
                item: Some(it.id().clone()),
                kind: Some("claim.broken".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(evs.len(), 1);

        // force break works regardless
        s.claim(it.id(), &a).unwrap();
        let broken = s.break_claim(it.id(), &b).unwrap();
        assert_eq!(broken.actor, a);
        assert!(matches!(
            s.release(it.id(), &b),
            Err(Error::NotClaimed { .. })
        ));
    }

    #[test]
    fn agents_register_heartbeat_and_deregister() {
        let (_t, s) = store();
        let a = s
            .register_agent(NewAgent {
                name: "coder-1".into(),
                profiles: vec!["coder".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(a.config.kind, AgentKind::Local);
        assert!(s.last_heartbeat(a.id()).unwrap().is_none());
        s.heartbeat(a.id()).unwrap();
        assert!(s.last_heartbeat(a.id()).unwrap().is_some());
        // resume keeps profiles when none given
        let again = s
            .register_agent(NewAgent {
                name: "coder-1".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(again.config.profiles, vec!["coder".to_string()]);
        assert_eq!(again.config.registered, a.config.registered);
        assert_eq!(s.list_agents().unwrap().len(), 1);
        // control plane and runtime are stored apart
        let mut cfg = again.config.clone();
        cfg.desired = Desired::Paused;
        cfg.hold = true;
        s.update_agent(&cfg, &ActorId::from("ops"), "paused coder-1")
            .unwrap();
        s.set_runtime(
            a.id(),
            &AgentRuntime {
                status: AgentStatus::Paused,
                ..Default::default()
            },
        )
        .unwrap();
        let got = s.get_agent(a.id()).unwrap();
        assert_eq!(got.config.desired, Desired::Paused);
        assert!(got.config.hold);
        assert_eq!(got.runtime.status, AgentStatus::Paused);
        s.deregister_agent(a.id()).unwrap();
        assert!(s.list_agents().unwrap().is_empty());
        assert!(matches!(s.get_agent(a.id()), Err(Error::AgentNotFound(_))));
    }

    #[test]
    fn deps_must_exist() {
        let (_t, s) = store();
        let err = s.create_item(
            "",
            NewItem {
                title: "x".into(),
                owner: ActorId::from("t"),
                deps: vec![ItemId::from("vf-missing")],
                ..Default::default()
            },
        );
        assert!(matches!(err, Err(Error::ItemNotFound(_))));
    }

    #[test]
    fn non_file_store_is_rejected_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collective::new("db");
        c.store.kind = StoreKind::Turso;
        c.store.url = Some("libsql://x".into());
        c.save(&tmp.path().join(CONFIG_FILE)).unwrap();
        let err = crate::store::open_store(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("turso"), "{err}");
    }

    #[test]
    fn git_store_commits_mutations() {
        if !git::is_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let c = Collective::new("gitted");
        let s = FileStore::init(&tmp.path().join(".vflt"), c).unwrap();
        let it = add(&s, "tracked", "code");
        s.add_note(it.id(), &ActorId::from("t"), "hello").unwrap();
        let out = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(s.root())
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(log.contains("init collective"), "{log}");
        assert!(log.contains("created vf-"), "{log}");
        assert!(log.contains("noted vf-"), "{log}");
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(s.root())
            .output()
            .unwrap();
        let dirty = String::from_utf8_lossy(&status.stdout);
        assert!(
            dirty.trim().is_empty(),
            "working tree should be clean, got:\n{dirty}"
        );
    }
}
