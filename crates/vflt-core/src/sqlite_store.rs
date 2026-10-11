//! The sqlite store: the whole board in one database file, for boards too big
//! or too busy for the file store. Profiles stay plain files beside it.
//!
//! ```text
//! collective.toml
//! board.db                   items, claims, notes, events, agents
//! profiles/<name>.{toml,md}
//! work/                      per-item workspaces and run logs
//! ```
//!
//! Every write runs in an immediate transaction, so a claim is atomic across
//! threads and processes, and a transition lands with its note and event or
//! not at all. Item state is stored as JSON (the source of truth) next to
//! indexed copies of the fields queries filter on.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::collective::{Collective, IdStyle, StoreKind};
use crate::discover::CONFIG_FILE;
use crate::error::{Error, Result};
use crate::file_store::{apply_update, join_ids, new_agent_config, normalize_newlines};
use crate::machine;
use crate::store::{write_skeleton, Store};
use crate::types::*;

pub const GITIGNORE: &str = "# vflt: the board database and per-item workspaces are machine-local\nwork/\n*.db\n*.db-wal\n*.db-shm\n";

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS items (
  id       TEXT PRIMARY KEY,
  seq      INTEGER,
  status   TEXT NOT NULL,
  stage    TEXT NOT NULL,
  assignee TEXT,
  owner    TEXT NOT NULL,
  priority INTEGER NOT NULL,
  key      TEXT,
  parent   TEXT,
  created  TEXT NOT NULL,
  updated  TEXT NOT NULL,
  state    TEXT NOT NULL,
  spec     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS items_status   ON items(status, stage);
CREATE INDEX IF NOT EXISTS items_assignee ON items(assignee, status);
CREATE INDEX IF NOT EXISTS items_owner    ON items(owner, status);
CREATE INDEX IF NOT EXISTS items_key      ON items(key) WHERE key IS NOT NULL;
CREATE INDEX IF NOT EXISTS items_parent   ON items(parent) WHERE parent IS NOT NULL;
CREATE INDEX IF NOT EXISTS items_seq      ON items(seq) WHERE seq IS NOT NULL;
CREATE TABLE IF NOT EXISTS claims (
  item           TEXT PRIMARY KEY REFERENCES items(id),
  actor          TEXT NOT NULL,
  at             TEXT NOT NULL,
  prior_assignee TEXT
);
CREATE TABLE IF NOT EXISTS notes (
  item  TEXT NOT NULL REFERENCES items(id),
  seq   INTEGER NOT NULL,
  actor TEXT NOT NULL,
  at    TEXT NOT NULL,
  body  TEXT NOT NULL,
  PRIMARY KEY (item, seq)
);
CREATE TABLE IF NOT EXISTS events (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts      TEXT NOT NULL,
  actor   TEXT NOT NULL,
  kind    TEXT NOT NULL,
  item    TEXT,
  summary TEXT NOT NULL,
  detail  TEXT
);
CREATE INDEX IF NOT EXISTS events_item ON events(item, id);
CREATE INDEX IF NOT EXISTS events_ts   ON events(ts);
CREATE INDEX IF NOT EXISTS events_kind ON events(kind, ts);
CREATE TABLE IF NOT EXISTS agents (
  id     TEXT PRIMARY KEY,
  config TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agent_runtime (
  id      TEXT PRIMARY KEY,
  runtime TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS heartbeats (
  id TEXT PRIMARY KEY,
  at TEXT NOT NULL
);
"#;

pub struct SqliteStore {
    root: PathBuf,
    data: PathBuf,
    collective: Collective,
    conn: Mutex<Connection>,
    mirror: Option<PathBuf>,
}

/// Timestamps are stored in one fixed RFC3339 shape so that text order is
/// time order.
fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| Error::invalid("timestamp", format!("{s:?}: {e}")))
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Other(format!("sqlite: {e}"))
}

fn db_path(root: &Path, coll: &Collective) -> PathBuf {
    let rel = coll
        .store
        .url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "board.db".to_string());
    let p = PathBuf::from(rel.strip_prefix("sqlite://").unwrap_or(&rel));
    if p.is_absolute() {
        p
    } else {
        root.join(p)
    }
}

fn connect(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(db_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
    )
    .map_err(db_err)?;
    conn.execute_batch(SCHEMA).map_err(db_err)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    match version.as_deref().map(str::parse::<i64>) {
        None => {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(db_err)?;
        }
        Some(Ok(v)) if v <= SCHEMA_VERSION => {}
        Some(_) => {
            return Err(Error::invalid(
                "store",
                format!(
                    "{} was written by a newer vflt (schema {:?}); upgrade vflt",
                    path.display(),
                    version
                ),
            ))
        }
    }
    Ok(conn)
}

impl SqliteStore {
    pub fn init(root: &Path, mut collective: Collective) -> Result<Self> {
        collective.store.kind = StoreKind::Sqlite;
        collective.store.git = false;
        let data = root.join(&collective.store.root);
        write_skeleton(root, &data, &collective, GITIGNORE)?;
        let store = Self::open(root)?;
        store.append_event(Event::new(
            &ActorId::new("vflt"),
            "collective.init",
            None,
            format!("initialised collective {}", store.collective.name),
        ))?;
        Ok(store)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let collective = Collective::load(&root.join(CONFIG_FILE))?;
        if collective.store.kind != StoreKind::Sqlite {
            return Err(Error::invalid(
                "store",
                format!(
                    "{} is a {} store",
                    root.display(),
                    collective.store.kind.as_str()
                ),
            ));
        }
        let data = root.join(&collective.store.root);
        let conn = connect(&db_path(root, &collective))?;
        let mirror = collective
            .store
            .mirror
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| root.join(m));
        Ok(SqliteStore {
            root: root.to_path_buf(),
            data,
            collective,
            conn: Mutex::new(conn),
            mirror,
        })
    }

    /// The configured mirror directory, if any.
    pub fn mirror_dir(&self) -> Option<&Path> {
        self.mirror.as_deref()
    }

    /// Rewrite the mirror file of `id`. Mirror trouble never fails the
    /// operation that caused it: the database is the source of truth.
    fn mirror_item(&self, id: &ItemId) {
        let Some(dir) = &self.mirror else { return };
        let res = self.read(|c| {
            let st = read_state(c, id)?;
            let spec = read_spec(c, id)?;
            let notes = read_notes(c, id)?;
            crate::mirror::render(&st, &spec, &notes)
        });
        match res.and_then(|text| crate::mirror::write(&crate::mirror::path_for(dir, id), &text)) {
            Ok(()) => {}
            Err(e) => eprintln!("vflt: warning: mirror of {id} not written: {e}"),
        }
    }

    /// Rewrite the mirror file of every item. Returns how many were written.
    pub fn rebuild_all_mirrors(&self) -> Result<usize> {
        let Some(dir) = self.mirror.clone() else {
            return Err(Error::invalid(
                "mirror",
                "store.mirror is not set in collective.toml",
            ));
        };
        let ids: Vec<ItemId> = self.read(|c| {
            let mut stmt = c.prepare("SELECT id FROM items").map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db_err)?;
            rows.map(|r| r.map(ItemId).map_err(db_err)).collect()
        })?;
        for id in &ids {
            let text = self.read(|c| {
                crate::mirror::render(&read_state(c, id)?, &read_spec(c, id)?, &read_notes(c, id)?)
            })?;
            crate::mirror::write(&crate::mirror::path_for(&dir, id), &text)?;
        }
        Ok(ids.len())
    }

    fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let out = f(&tx)?;
        tx.commit().map_err(db_err)?;
        Ok(out)
    }

    fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }
}

// -- row helpers -------------------------------------------------------------

fn read_state(c: &Connection, id: &ItemId) -> Result<ItemState> {
    let json: Option<String> = c
        .query_row(
            "SELECT state FROM items WHERE id = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    match json {
        Some(j) => Ok(serde_json::from_str(&j)?),
        None => Err(Error::ItemNotFound(id.to_string())),
    }
}

fn read_spec(c: &Connection, id: &ItemId) -> Result<String> {
    c.query_row(
        "SELECT spec FROM items WHERE id = ?1",
        params![id.as_str()],
        |r| r.get(0),
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| Error::ItemNotFound(id.to_string()))
}

fn read_claim(c: &Connection, id: &ItemId) -> Result<Option<Claim>> {
    c.query_row(
        "SELECT actor, at, prior_assignee FROM claims WHERE item = ?1",
        params![id.as_str()],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    )
    .optional()
    .map_err(db_err)?
    .map(|(actor, at, prior)| {
        Ok(Claim {
            item: id.clone(),
            actor: ActorId(actor),
            at: parse_ts(&at)?,
            prior_assignee: prior.filter(|p| !p.is_empty()).map(ActorId),
        })
    })
    .transpose()
}

fn exists(c: &Connection, id: &ItemId) -> Result<bool> {
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM items WHERE id = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(n > 0)
}

fn insert_item(c: &Connection, st: &ItemState, spec: &str) -> Result<()> {
    c.execute(
        "INSERT INTO items (id, seq, status, stage, assignee, owner, priority, key, parent, created, updated, state, spec)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            st.id.as_str(),
            st.id.seq_number().map(|n| n as i64),
            st.status.as_str(),
            st.stage.as_str(),
            st.assignee.as_ref().map(|a| a.as_str()),
            st.owner.as_str(),
            st.priority,
            st.key.as_deref(),
            st.parent.as_ref().map(|p| p.as_str()),
            ts(&st.created),
            ts(&st.updated),
            serde_json::to_string(st)?,
            spec,
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
            Error::invalid("item", format!("{} already exists", st.id))
        }
        other => db_err(other),
    })?;
    Ok(())
}

fn write_state(c: &Connection, st: &ItemState) -> Result<()> {
    let n = c
        .execute(
            "UPDATE items SET status = ?2, stage = ?3, assignee = ?4, owner = ?5, priority = ?6, key = ?7,
                    parent = ?8, created = ?9, updated = ?10, state = ?11 WHERE id = ?1",
            params![
                st.id.as_str(),
                st.status.as_str(),
                st.stage.as_str(),
                st.assignee.as_ref().map(|a| a.as_str()),
                st.owner.as_str(),
                st.priority,
                st.key.as_deref(),
                st.parent.as_ref().map(|p| p.as_str()),
                ts(&st.created),
                ts(&st.updated),
                serde_json::to_string(st)?,
            ],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(Error::ItemNotFound(st.id.to_string()));
    }
    Ok(())
}

fn insert_event(c: &Connection, e: &Event) -> Result<()> {
    c.execute(
        "INSERT INTO events (ts, actor, kind, item, summary, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ts(&e.ts),
            e.actor.as_str(),
            e.kind,
            e.item.as_ref().map(|i| i.as_str()),
            e.summary,
            if e.detail.is_null() {
                None
            } else {
                Some(serde_json::to_string(&e.detail)?)
            },
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

fn next_note_seq(c: &Connection, id: &ItemId) -> Result<u32> {
    let n: i64 = c
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM notes WHERE item = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(n as u32)
}

fn insert_note(c: &Connection, id: &ItemId, n: &Note) -> Result<()> {
    c.execute(
        "INSERT INTO notes (item, seq, actor, at, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.as_str(), n.seq, n.actor.as_str(), ts(&n.at), n.body],
    )
    .map_err(db_err)?;
    Ok(())
}

fn new_note(c: &Connection, id: &ItemId, by: &ActorId, body: &str) -> Result<Note> {
    let note = Note {
        seq: next_note_seq(c, id)?,
        actor: by.clone(),
        at: Utc::now(),
        body: normalize_newlines(body.trim_end()).trim_end().to_string(),
    };
    insert_note(c, id, &note)?;
    Ok(note)
}

fn read_notes(c: &Connection, id: &ItemId) -> Result<Vec<Note>> {
    let mut stmt = c
        .prepare("SELECT seq, actor, at, body FROM notes WHERE item = ?1 ORDER BY seq")
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![id.as_str()], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(db_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, actor, at, body) = row.map_err(db_err)?;
        out.push(Note {
            seq,
            actor: ActorId(actor),
            at: parse_ts(&at)?,
            body,
        });
    }
    Ok(out)
}

fn check_deps(c: &Connection, id: Option<&ItemId>, deps: &[ItemId]) -> Result<()> {
    for d in deps {
        if !exists(c, d)? {
            return Err(Error::ItemNotFound(format!("dependency {d}")));
        }
    }
    if let Some(id) = id {
        if machine::creates_cycle(id, deps, |d| Ok(read_state(c, d)?.deps))? {
            return Err(Error::invalid(
                "deps",
                format!("depending on {} would create a cycle", join_ids(deps)),
            ));
        }
    }
    Ok(())
}

fn load_item(c: &Connection, st: ItemState, with_spec: bool) -> Result<Item> {
    let spec = if with_spec {
        read_spec(c, &st.id)?
    } else {
        String::new()
    };
    let claim = read_claim(c, &st.id)?;
    Ok(Item {
        state: st,
        spec,
        claim,
    })
}

fn read_agent_config(c: &Connection, id: &ActorId) -> Result<AgentConfig> {
    let json: Option<String> = c
        .query_row(
            "SELECT config FROM agents WHERE id = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    match json {
        Some(j) => Ok(serde_json::from_str(&j)?),
        None => Err(Error::AgentNotFound(id.to_string())),
    }
}

fn assemble_agent(c: &Connection, config: AgentConfig) -> Result<Agent> {
    let rt: Option<String> = c
        .query_row(
            "SELECT runtime FROM agent_runtime WHERE id = ?1",
            params![config.id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let runtime = rt
        .map(|j| serde_json::from_str(&j).unwrap_or_default())
        .unwrap_or_default();
    let hb: Option<String> = c
        .query_row(
            "SELECT at FROM heartbeats WHERE id = ?1",
            params![config.id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    Ok(Agent {
        config,
        runtime,
        last_seen: hb.map(|s| parse_ts(&s)).transpose()?,
    })
}

fn write_agent_config(c: &Connection, cfg: &AgentConfig) -> Result<()> {
    c.execute(
        "INSERT INTO agents (id, config) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET config = excluded.config",
        params![cfg.id.as_str(), serde_json::to_string(cfg)?],
    )
    .map_err(db_err)?;
    Ok(())
}

/// An event row as stored: ts, actor, kind, item, summary, detail.
type EventRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
);

fn row_event(r: &rusqlite::Row) -> rusqlite::Result<EventRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn to_event((t, actor, kind, item, summary, detail): EventRow) -> Result<Event> {
    Ok(Event {
        ts: parse_ts(&t)?,
        actor: ActorId(actor),
        kind,
        item: item.map(ItemId),
        summary,
        detail: match detail {
            Some(d) => serde_json::from_str(&d)?,
            None => serde_json::Value::Null,
        },
    })
}

fn not_holder_or_stale(
    store: &SqliteStore,
    c: &Connection,
    claim: &Claim,
    by: &ActorId,
) -> Result<&'static str> {
    if &claim.actor == by {
        return Ok("item.released");
    }
    // Stale: the holder is a registered agent whose heartbeat is older than
    // the claim ttl. Evaluated on this connection to stay inside the lock.
    let ttl = chrono::Duration::from_std(store.collective.claim_ttl())
        .map_err(|e| Error::invalid("claim_ttl", e.to_string()))?;
    let registered = read_agent_config(c, &claim.actor).is_ok();
    let hb: Option<String> = c
        .query_row(
            "SELECT at FROM heartbeats WHERE id = ?1",
            params![claim.actor.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let last = hb
        .map(|s| parse_ts(&s))
        .transpose()?
        .unwrap_or(claim.at)
        .max(claim.at);
    if registered && Utc::now() - last > ttl {
        Ok("claim.broken")
    } else {
        Err(Error::NotClaimHolder {
            item: claim.item.to_string(),
            holder: claim.actor.to_string(),
            actor: by.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------

impl Store for SqliteStore {
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
        let coll = &self.collective;
        let item = self.write(|tx| {
            check_deps(tx, None, &meta.deps)?;
            if let Some(p) = &meta.parent {
                if !exists(tx, p)? {
                    return Err(Error::ItemNotFound(format!("parent {p}")));
                }
            }
            if let Some(k) = &meta.key {
                let open: Option<String> = tx
                    .query_row(
                        "SELECT id FROM items WHERE key = ?1 AND status NOT IN ('done','cancelled')",
                        params![k],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db_err)?;
                if let Some(o) = open {
                    return Err(Error::invalid("key", format!("{o} already carries key {k:?}")));
                }
            }
            let id = match coll.ids {
                IdStyle::Ulid => ItemId::generate(),
                IdStyle::Seq => {
                    let n: i64 = tx
                        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM items", [], |r| r.get(0))
                        .map_err(db_err)?;
                    ItemId::seq(n as u64)
                }
            };
            let now = Utc::now();
            let stage = meta
                .stage
                .clone()
                .unwrap_or_else(|| coll.pipeline.first().cloned().unwrap_or(Stage::new(Stage::CODE)));
            let st = ItemState {
                id: id.clone(),
                slug: item_slug(meta.slug.as_deref(), &meta.title),
                title: meta.title.trim().to_string(),
                origin: machine::origin_for_new(&stage, &meta.owner, meta.assignee.as_ref()),
                stage,
                status: if meta.draft { Status::Draft } else { Status::Pending },
                priority: meta.priority.unwrap_or_else(default_priority),
                owner: meta.owner.clone(),
                assignee: meta.assignee.clone(),
                tags: normalize_tags(meta.tags.clone()),
                deps: meta.deps.clone(),
                created: now,
                updated: now,
                closed: None,
                external: meta.external.clone(),
                waiting_on: None,
                raise_kind: None,
                parent: meta.parent.clone(),
                bounces: 0,
                returned: false,
                summary: meta.summary.clone().filter(|s| !s.trim().is_empty()),
                key: meta.key.clone(),
            };
            let spec_text = if spec.trim().is_empty() {
                format!("# {}\n", st.title)
            } else {
                normalize_newlines(spec)
            };
            insert_item(tx, &st, &spec_text)?;
            insert_event(
                tx,
                &Event::new(&meta.owner, "item.created", Some(&id), format!("created {id} \"{}\" at {}", st.title, st.stage))
                    .with_detail(serde_json::json!({"stage": st.stage, "status": st.status, "priority": st.priority, "assignee": st.assignee})),
            )?;
            Ok(Item { state: st, spec: spec_text, claim: None })
        })?;
        self.mirror_item(item.id());
        Ok(item)
    }

    fn get_item(&self, id: &ItemId) -> Result<Item> {
        self.read(|c| {
            let st = read_state(c, id)?;
            load_item(c, st, true)
        })
    }

    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>> {
        self.read(|c| {
            let mut sql = String::from(
                "SELECT i.state, cl.actor, cl.at, cl.prior_assignee FROM items i LEFT JOIN claims cl ON cl.item = i.id WHERE 1=1",
            );
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if !filter.statuses.is_empty() {
                sql.push_str(" AND i.status IN (");
                for (n, s) in filter.statuses.iter().enumerate() {
                    if n > 0 {
                        sql.push(',');
                    }
                    sql.push('?');
                    args.push(Box::new(s.as_str().to_string()));
                }
                sql.push(')');
            }
            if let Some(s) = &filter.stage {
                sql.push_str(" AND i.stage = ?");
                args.push(Box::new(s.as_str().to_string()));
            }
            if let Some(a) = &filter.assignee {
                sql.push_str(" AND i.assignee = ?");
                args.push(Box::new(a.as_str().to_string()));
            }
            if let Some(o) = &filter.owner {
                sql.push_str(" AND i.owner = ?");
                args.push(Box::new(o.as_str().to_string()));
            }
            if let Some(k) = &filter.key {
                sql.push_str(" AND i.key = ?");
                args.push(Box::new(k.clone()));
            }
            if let Some(p) = &filter.parent {
                sql.push_str(" AND i.parent = ?");
                args.push(Box::new(p.as_str().to_string()));
            }
            if let Some(t) = &filter.updated_before {
                sql.push_str(" AND i.updated < ?");
                args.push(Box::new(ts(t)));
            }
            sql.push_str(" ORDER BY i.priority, i.created");
            let mut stmt = c.prepare(&sql).map_err(db_err)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(db_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (state, actor, at, prior) = row.map_err(db_err)?;
                let st: ItemState = serde_json::from_str(&state)?;
                if !filter.matches(&st) {
                    continue;
                }
                let claim = match (actor, at) {
                    (Some(actor), Some(at)) => Some(Claim {
                        item: st.id.clone(),
                        actor: ActorId(actor),
                        at: parse_ts(&at)?,
                        prior_assignee: prior.filter(|p| !p.is_empty()).map(ActorId),
                    }),
                    _ => None,
                };
                out.push(Item { state: st, spec: String::new(), claim });
            }
            Ok(out)
        })
    }

    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim> {
        let out = self.write(|tx| {
            let mut st = read_state(tx, id)?;
            if let Some(c) = read_claim(tx, id)? {
                return Err(Error::AlreadyClaimed {
                    item: id.to_string(),
                    holder: c.actor.to_string(),
                });
            }
            if st.status != Status::Pending {
                return Err(Error::InvalidTransition {
                    item: id.to_string(),
                    verb: "claim",
                    from: st.status.to_string(),
                });
            }
            let now = Utc::now();
            let claim = Claim {
                item: id.clone(),
                actor: by.clone(),
                at: now,
                prior_assignee: st.assignee.clone(),
            };
            tx.execute(
                "INSERT INTO claims (item, actor, at, prior_assignee) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.as_str(),
                    by.as_str(),
                    ts(&now),
                    claim.prior_assignee.as_ref().map(|a| a.as_str())
                ],
            )
            .map_err(db_err)?;
            st.status = Status::Claimed;
            st.assignee = Some(by.clone());
            st.returned = false;
            st.updated = now;
            write_state(tx, &st)?;
            insert_event(
                tx,
                &Event::new(by, "item.claimed", Some(id), format!("{by} claimed {id}")),
            )?;
            Ok(claim)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()> {
        self.write(|tx| {
            let claim = read_claim(tx, id)?.ok_or_else(|| Error::NotClaimed {
                item: id.to_string(),
            })?;
            let kind = not_holder_or_stale(self, tx, &claim, by)?;
            tx.execute("DELETE FROM claims WHERE item = ?1", params![id.as_str()])
                .map_err(db_err)?;
            let mut st = read_state(tx, id)?;
            if matches!(st.status, Status::Claimed | Status::InProgress) {
                st.status = Status::Pending;
                st.assignee = claim.prior_assignee.clone();
                st.updated = Utc::now();
            }
            write_state(tx, &st)?;
            let summary = if kind == "claim.broken" {
                format!("{by} broke stale claim on {id} held by {}", claim.actor)
            } else {
                format!("{by} released {id}")
            };
            insert_event(tx, &Event::new(by, kind, Some(id), summary))
        })?;
        self.mirror_item(id);
        Ok(())
    }

    fn break_claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim> {
        let out = self.write(|tx| {
            let claim = read_claim(tx, id)?.ok_or_else(|| Error::NotClaimed {
                item: id.to_string(),
            })?;
            tx.execute("DELETE FROM claims WHERE item = ?1", params![id.as_str()])
                .map_err(db_err)?;
            let mut st = read_state(tx, id)?;
            if matches!(st.status, Status::Claimed | Status::InProgress) {
                st.status = Status::Pending;
                st.assignee = claim.prior_assignee.clone();
                st.updated = Utc::now();
            }
            write_state(tx, &st)?;
            insert_event(
                tx,
                &Event::new(
                    by,
                    "claim.broken",
                    Some(id),
                    format!("{by} forcibly broke claim on {id} held by {}", claim.actor),
                ),
            )?;
            Ok(claim)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item> {
        if to == Transition::Start {
            let st = self.read(|c| read_state(c, id))?;
            if st.status == Status::Pending {
                // implicit claim
                self.claim(id, by)?;
            }
        }
        let out = self.write(|tx| {
            let mut st = read_state(tx, id)?;
            if let Transition::Wait { on } = &to {
                check_deps(tx, Some(id), on)?;
            }
            let claim = read_claim(tx, id)?;
            let applied = machine::apply(
                &self.collective,
                &mut st,
                claim.as_ref(),
                by,
                to,
                Utc::now(),
            )?;
            if applied.drop_claim {
                tx.execute("DELETE FROM claims WHERE item = ?1", params![id.as_str()])
                    .map_err(db_err)?;
            }
            write_state(tx, &st)?;
            let mut detail = applied.detail;
            if let Some(body) = &applied.note {
                let n = new_note(tx, id, by, body)?;
                detail["note_seq"] = serde_json::json!(n.seq);
            }
            insert_event(
                tx,
                &Event::new(by, applied.kind, Some(id), applied.summary).with_detail(detail),
            )?;
            load_item(tx, st, false)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn update_item(&self, id: &ItemId, by: &ActorId, u: ItemUpdate) -> Result<Item> {
        let out = self.write(|tx| {
            let mut st = read_state(tx, id)?;
            if u.is_empty() {
                return load_item(tx, st, true);
            }
            let mut changes: Vec<String> = Vec::new();
            if !u.add_deps.is_empty() {
                check_deps(tx, Some(id), &u.add_deps)?;
            }
            if let Some(Some(p)) = &u.parent {
                if !exists(tx, p)? {
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
                tx.execute(
                    "UPDATE items SET spec = ?2 WHERE id = ?1",
                    params![id.as_str(), normalize_newlines(spec)],
                )
                .map_err(db_err)?;
                changes.push("spec".into());
            }
            apply_update(&mut st, u, &mut changes);
            st.updated = Utc::now();
            write_state(tx, &st)?;
            insert_event(
                tx,
                &Event::new(
                    by,
                    "item.updated",
                    Some(id),
                    format!("{by} updated {id}: {}", changes.join(", ")),
                )
                .with_detail(serde_json::json!({"changes": changes})),
            )?;
            load_item(tx, st, true)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note> {
        let out = self.write(|tx| {
            read_state(tx, id)?;
            let note = new_note(tx, id, by, body)?;
            insert_event(
                tx,
                &Event::new(
                    by,
                    "item.noted",
                    Some(id),
                    format!("{by} noted {id} (#{})", note.seq),
                )
                .with_detail(serde_json::json!({"seq": note.seq})),
            )?;
            Ok(note)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn notes(&self, id: &ItemId) -> Result<Vec<Note>> {
        self.read(|c| read_notes(c, id))
    }

    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item> {
        let out = self.write(|tx| {
            let mut st = read_state(tx, id)?;
            st.assignee = to.clone();
            st.updated = Utc::now();
            write_state(tx, &st)?;
            let summary = match &to {
                Some(a) => format!("{by} assigned {id} to {a}"),
                None => format!("{by} returned {id} to the pool"),
            };
            insert_event(tx, &Event::new(by, "item.assigned", Some(id), summary))?;
            load_item(tx, st, false)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item> {
        let out = self.write(|tx| {
            let mut st = read_state(tx, id)?;
            let summary = format!("{by} linked {id} to {}:{}", ext.system, ext.key);
            st.external = Some(ext);
            st.updated = Utc::now();
            write_state(tx, &st)?;
            insert_event(tx, &Event::new(by, "item.linked", Some(id), summary))?;
            load_item(tx, st, false)
        })?;
        self.mirror_item(id);
        Ok(out)
    }

    // -- agents -------------------------------------------------------------

    fn register_agent(&self, a: NewAgent) -> Result<Agent> {
        let name = a.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::invalid("agent", "name is empty"));
        }
        let id = ActorId::new(&name);
        self.write(|tx| {
            let existing = read_agent_config(tx, &id).ok();
            let cfg = new_agent_config(&id, a, existing.as_ref());
            write_agent_config(tx, &cfg)?;
            let resumed = existing.is_some();
            insert_event(
                tx,
                &Event::new(
                    &id,
                    if resumed {
                        "agent.resumed"
                    } else {
                        "agent.registered"
                    },
                    None,
                    format!(
                        "agent {id} {} with profiles {:?}",
                        if resumed { "resumed" } else { "registered" },
                        cfg.profiles
                    ),
                ),
            )?;
            assemble_agent(tx, cfg)
        })
    }

    fn get_agent(&self, id: &ActorId) -> Result<Agent> {
        self.read(|c| {
            let cfg = read_agent_config(c, id)?;
            assemble_agent(c, cfg)
        })
    }

    fn update_agent(&self, cfg: &AgentConfig, by: &ActorId, summary: &str) -> Result<()> {
        self.write(|tx| {
            read_agent_config(tx, &cfg.id)?;
            write_agent_config(tx, cfg)?;
            insert_event(
                tx,
                &Event::new(by, "agent.updated", None, summary.to_string()),
            )
        })
    }

    fn set_runtime(&self, id: &ActorId, rt: &AgentRuntime) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO agent_runtime (id, runtime) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET runtime = excluded.runtime",
                params![id.as_str(), serde_json::to_string(rt)?],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn heartbeat(&self, id: &ActorId) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO heartbeats (id, at) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET at = excluded.at",
                params![id.as_str(), ts(&Utc::now())],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn last_heartbeat(&self, id: &ActorId) -> Result<Option<DateTime<Utc>>> {
        self.read(|c| {
            let hb: Option<String> = c
                .query_row(
                    "SELECT at FROM heartbeats WHERE id = ?1",
                    params![id.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            hb.map(|s| parse_ts(&s)).transpose()
        })
    }

    fn list_agents(&self) -> Result<Vec<Agent>> {
        self.read(|c| {
            let mut stmt = c
                .prepare("SELECT config FROM agents ORDER BY id")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db_err)?;
            let mut out = Vec::new();
            for row in rows {
                let cfg: AgentConfig = serde_json::from_str(&row.map_err(db_err)?)?;
                out.push(assemble_agent(c, cfg)?);
            }
            out.sort_by(|a, b| a.config.name.cmp(&b.config.name));
            Ok(out)
        })
    }

    fn deregister_agent(&self, id: &ActorId) -> Result<()> {
        self.write(|tx| {
            read_agent_config(tx, id)?;
            for table in ["agents", "agent_runtime", "heartbeats"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE id = ?1"),
                    params![id.as_str()],
                )
                .map_err(db_err)?;
            }
            insert_event(
                tx,
                &Event::new(
                    id,
                    "agent.deregistered",
                    None,
                    format!("agent {id} deregistered"),
                ),
            )
        })
    }

    // -- events -------------------------------------------------------------

    fn append_event(&self, e: Event) -> Result<()> {
        self.write(|tx| insert_event(tx, &e))
    }

    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>> {
        self.read(|c| {
            let mut sql = String::from(
                "SELECT ts, actor, kind, item, summary, detail, id FROM events WHERE 1=1",
            );
            let mut args: Vec<String> = Vec::new();
            if let Some(i) = &filter.item {
                sql.push_str(" AND item = ?");
                args.push(i.as_str().to_string());
            }
            if let Some(s) = &filter.since {
                sql.push_str(" AND ts >= ?");
                args.push(ts(s));
            }
            if let Some(k) = &filter.kind {
                sql.push_str(" AND kind = ?");
                args.push(k.clone());
            }
            if let Some(a) = &filter.actor {
                sql.push_str(" AND actor = ?");
                args.push(a.as_str().to_string());
            }
            match filter.limit {
                Some(n) => {
                    sql = format!("SELECT * FROM ({sql} ORDER BY id DESC LIMIT {n}) ORDER BY id")
                }
                None => sql.push_str(" ORDER BY id"),
            }
            let mut stmt = c.prepare(&sql).map_err(db_err)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), row_event)
                .map_err(db_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(to_event(row.map_err(db_err)?)?);
            }
            Ok(out)
        })
    }

    // -- bundles ------------------------------------------------------------

    fn import_record(&self, rec: BundleRecord) -> Result<()> {
        let touched = match &rec {
            BundleRecord::Item { state, .. } => Some(state.id.clone()),
            BundleRecord::Note { item, .. } => Some(item.clone()),
            _ => None,
        };
        self.write(|tx| match rec {
            BundleRecord::Item { state, spec } => {
                insert_item(tx, &state, &normalize_newlines(&spec))
            }
            BundleRecord::Note { item, note } => {
                if !exists(tx, &item)? {
                    return Err(Error::ItemNotFound(item.to_string()));
                }
                insert_note(tx, &item, &note)
            }
            BundleRecord::Event(e) => insert_event(tx, &e),
            BundleRecord::Agent(cfg) => write_agent_config(tx, &cfg),
        })?;
        if let Some(id) = touched {
            self.mirror_item(&id);
        }
        Ok(())
    }

    fn export_records(&self) -> Result<Vec<BundleRecord>> {
        self.read(|c| {
            let mut out = Vec::new();
            {
                let mut stmt = c
                    .prepare("SELECT state, spec FROM items ORDER BY created, id")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(db_err)?;
                for row in rows {
                    let (state, spec) = row.map_err(db_err)?;
                    out.push(BundleRecord::Item {
                        state: serde_json::from_str(&state)?,
                        spec,
                    });
                }
            }
            {
                let mut stmt = c
                    .prepare("SELECT item, seq, actor, at, body FROM notes ORDER BY item, seq")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(db_err)?;
                for row in rows {
                    let (item, seq, actor, at, body) = row.map_err(db_err)?;
                    out.push(BundleRecord::Note {
                        item: ItemId(item),
                        note: Note {
                            seq,
                            actor: ActorId(actor),
                            at: parse_ts(&at)?,
                            body,
                        },
                    });
                }
            }
            {
                let mut stmt = c
                    .prepare(
                        "SELECT ts, actor, kind, item, summary, detail FROM events ORDER BY id",
                    )
                    .map_err(db_err)?;
                let rows = stmt.query_map([], row_event).map_err(db_err)?;
                for row in rows {
                    out.push(BundleRecord::Event(to_event(row.map_err(db_err)?)?));
                }
            }
            {
                let mut stmt = c
                    .prepare("SELECT config FROM agents ORDER BY id")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(db_err)?;
                for row in rows {
                    out.push(BundleRecord::Agent(serde_json::from_str(
                        &row.map_err(db_err)?,
                    )?));
                }
            }
            Ok(out)
        })
    }

    fn rebuild_mirror(&self) -> Result<usize> {
        self.rebuild_all_mirrors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn store(ids: IdStyle) -> (tempfile::TempDir, SqliteStore) {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collective::new("test");
        c.store = crate::collective::StoreConfig::sqlite();
        c.ids = ids;
        let s = SqliteStore::init(&tmp.path().join(".vflt"), c).unwrap();
        (tmp, s)
    }

    fn add(s: &SqliteStore, title: &str, stage: &str) -> Item {
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
    fn init_writes_db_profiles_and_ignore_file() {
        let (_t, s) = store(IdStyle::Ulid);
        assert!(s.root().join("board.db").is_file());
        assert!(s.root().join("collective.toml").is_file());
        assert!(std::fs::read_to_string(s.root().join(".gitignore"))
            .unwrap()
            .contains("*.db"));
        assert!(!s.profiles().unwrap().is_empty());
        let reopened = crate::store::open_store(s.root()).unwrap();
        assert_eq!(reopened.collective().unwrap().store.kind, StoreKind::Sqlite);
        let evs = s.events(&EventFilter::default()).unwrap();
        assert_eq!(evs[0].kind, "collective.init");
    }

    #[test]
    fn sequential_ids_count_up_and_survive_import() {
        let (_t, s) = store(IdStyle::Seq);
        assert_eq!(add(&s, "a", "code").id().as_str(), "vf-1");
        assert_eq!(add(&s, "b", "code").id().as_str(), "vf-2");
        let mut st = s.get_item(&ItemId::from("vf-2")).unwrap().state;
        st.id = ItemId::seq(40);
        s.import_record(BundleRecord::Item {
            state: st,
            spec: "imported".into(),
        })
        .unwrap();
        assert_eq!(add(&s, "c", "code").id().as_str(), "vf-41");
        assert_eq!(s.get_item(&ItemId::seq(40)).unwrap().spec, "imported\n");
    }

    #[test]
    fn crud_filters_and_lazy_specs() {
        let (_t, s) = store(IdStyle::Ulid);
        let a = add(&s, "Write the thing", "code");
        let mut b = add(&s, "Review the thing", "review");
        b = s
            .update_item(
                b.id(),
                &ActorId::from("x"),
                ItemUpdate {
                    add_tags: vec!["Area:Core".into()],
                    priority: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(b.tags, vec!["area:core".to_string()]);
        let all = s.list_items(&ItemFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id(), b.id(), "priority 5 sorts first");
        assert!(all.iter().all(|i| i.spec.is_empty()));
        assert_eq!(s.get_item(a.id()).unwrap().spec, "spec body\n");
        let tagged = s
            .list_items(&ItemFilter {
                tags: vec!["area:core".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tagged.len(), 1);
        let pending_code = s
            .list_items(&ItemFilter {
                statuses: vec![Status::Pending],
                stage: Some(Stage::new("code")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pending_code.len(), 1);
        let n = s
            .add_note(a.id(), &ActorId::from("t"), "first\r\nnote")
            .unwrap();
        assert_eq!(n.seq, 1);
        assert_eq!(s.notes(a.id()).unwrap()[0].body, "first\nnote");
        let evs = s
            .events(&EventFilter {
                item: Some(a.id().clone()),
                ..Default::default()
            })
            .unwrap();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["item.created", "item.noted"]);
        let last = s
            .events(&EventFilter {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].kind, "item.noted");
    }

    #[test]
    fn claim_race_has_exactly_one_winner() {
        let (_t, s) = store(IdStyle::Ulid);
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
        // a second process-level connection sees the same claim
        let other = SqliteStore::open(s.root()).unwrap();
        assert!(matches!(
            other.claim(&id, &ActorId::from("late")),
            Err(Error::AlreadyClaimed { .. })
        ));
    }

    #[test]
    fn transitions_flags_drafts_and_waits() {
        let (_t, s) = store(IdStyle::Seq);
        let lead = ActorId::from("lead");
        let tester = ActorId::from("tester");
        let probe = s
            .create_item(
                "check it",
                NewItem {
                    title: "probe".into(),
                    stage: Some(Stage::new("code")),
                    owner: lead.clone(),
                    assignee: Some(tester.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        s.transition(probe.id(), &tester, Transition::Start)
            .unwrap();
        let bounced = s
            .transition(
                probe.id(),
                &tester,
                Transition::Complete {
                    bounce: true,
                    to_stage: None,
                    note: Some("fails at step B".into()),
                },
            )
            .unwrap();
        assert_eq!(bounced.assignee.as_ref(), Some(&lead));
        assert_eq!(bounced.bounces, 1);
        assert_eq!(s.notes(probe.id()).unwrap().len(), 1);

        let parent = add(&s, "parent", "code");
        s.transition(parent.id(), &lead, Transition::Start).unwrap();
        let waiting = s
            .transition(
                parent.id(),
                &lead,
                Transition::Wait {
                    on: vec![probe.id().clone()],
                },
            )
            .unwrap();
        assert_eq!(waiting.status, Status::Pending);
        assert_eq!(waiting.deps, vec![probe.id().clone()]);
        assert!(
            s.transition(
                probe.id(),
                &lead,
                Transition::Wait {
                    on: vec![parent.id().clone()]
                }
            )
            .is_err(),
            "cycle refused"
        );

        let flag = s
            .create_item(
                "",
                NewItem {
                    title: "disk low".into(),
                    owner: lead.clone(),
                    key: Some("disk".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(s
            .create_item(
                "",
                NewItem {
                    title: "again".into(),
                    owner: lead.clone(),
                    key: Some("disk".into()),
                    ..Default::default()
                }
            )
            .is_err());
        assert_eq!(s.open_by_key("disk").unwrap().unwrap().id(), flag.id());
        s.transition(flag.id(), &lead, Transition::Cancel).unwrap();
        assert!(s.open_by_key("disk").unwrap().is_none());

        let draft = s
            .create_item(
                "v1",
                NewItem {
                    title: "draft".into(),
                    owner: lead.clone(),
                    draft: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(draft.status, Status::Draft);
        assert!(s.claim(draft.id(), &tester).is_err());
        let edited = s
            .update_item(
                draft.id(),
                &lead,
                ItemUpdate {
                    spec: Some("v2".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(edited.spec, "v2\n");
        s.transition(draft.id(), &lead, Transition::Promote)
            .unwrap();
        assert!(s
            .update_item(
                draft.id(),
                &lead,
                ItemUpdate {
                    spec: Some("v3".into()),
                    ..Default::default()
                }
            )
            .is_err());
    }

    #[test]
    fn agents_config_runtime_and_stale_claims() {
        let (_t, s) = store(IdStyle::Ulid);
        let a = ActorId::from("agent-a");
        let b = ActorId::from("agent-b");
        s.register_agent(NewAgent {
            name: "agent-a".into(),
            profiles: vec!["coder".into()],
            ..Default::default()
        })
        .unwrap();
        let it = add(&s, "x", "code");
        s.heartbeat(&a).unwrap();
        s.claim(it.id(), &a).unwrap();
        assert!(matches!(
            s.release(it.id(), &b),
            Err(Error::NotClaimHolder { .. })
        ));
        // age the heartbeat and the claim past the ttl
        let old = ts(&(Utc::now() - chrono::Duration::hours(2)));
        s.read(|c| {
            c.execute("UPDATE heartbeats SET at = ?1", params![old])
                .unwrap();
            c.execute("UPDATE claims SET at = ?1", params![old])
                .unwrap();
            Ok(())
        })
        .unwrap();
        s.release(it.id(), &b).unwrap();
        let broken = s
            .events(&EventFilter {
                kind: Some("claim.broken".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(broken.len(), 1);

        let mut cfg = s.get_agent(&a).unwrap().config;
        cfg.desired = Desired::Paused;
        cfg.model = Some("m".into());
        s.update_agent(&cfg, &ActorId::from("ops"), "pause")
            .unwrap();
        s.set_runtime(
            &a,
            &AgentRuntime {
                status: AgentStatus::Paused,
                note: Some("paused".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let got = s.get_agent(&a).unwrap();
        assert_eq!(got.config.desired, Desired::Paused);
        assert_eq!(got.config.model.as_deref(), Some("m"));
        assert_eq!(got.runtime.status, AgentStatus::Paused);
        assert!(got.last_seen.is_some());
        s.deregister_agent(&a).unwrap();
        assert!(s.list_agents().unwrap().is_empty());
    }

    #[test]
    fn mirror_follows_every_change_and_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collective::new("m");
        c.store = crate::collective::StoreConfig::sqlite();
        c.store.mirror = Some("board".into());
        c.ids = IdStyle::Seq;
        let s = SqliteStore::init(&tmp.path().join(".vflt"), c).unwrap();
        let it = add(&s, "mirrored", "code");
        let path = crate::mirror::path_for(s.mirror_dir().unwrap(), it.id());
        assert!(path.is_file());
        s.add_note(it.id(), &ActorId::from("t"), "## Findings\n\nGO")
            .unwrap();
        s.transition(it.id(), &ActorId::from("t"), Transition::Start)
            .unwrap();
        let (st, spec, notes) =
            crate::mirror::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(st.status, Status::InProgress);
        assert_eq!(spec, "spec body\n");
        assert_eq!(notes[0].body, "## Findings\n\nGO");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(s.rebuild_all_mirrors().unwrap(), 1);
        assert!(path.is_file());
    }

    #[test]
    fn export_import_round_trip_between_stores() {
        let (_t, s) = store(IdStyle::Seq);
        let it = add(&s, "carry me", "code");
        s.add_note(it.id(), &ActorId::from("t"), "a note").unwrap();
        s.register_agent(NewAgent {
            name: "w".into(),
            profiles: vec!["coder".into()],
            ..Default::default()
        })
        .unwrap();
        let records = s.export_records().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collective::new("files");
        c.store.git = false;
        let f = crate::file_store::FileStore::init(&tmp.path().join(".vflt"), c).unwrap();
        for r in records {
            f.import_record(r).unwrap();
        }
        let got = f.get_item(it.id()).unwrap();
        assert_eq!(got.title, "carry me");
        assert_eq!(got.spec, "spec body\n");
        assert_eq!(f.notes(it.id()).unwrap()[0].body, "a note");
        assert_eq!(f.list_agents().unwrap().len(), 1);
        assert!(
            f.events(&EventFilter {
                item: Some(it.id().clone()),
                ..Default::default()
            })
            .unwrap()
            .len()
                >= 2
        );
    }
}
