//! The plain-file mirror of a database-backed board: one markdown file per
//! item, holding its state, spec and notes. A store with `store.mirror` set
//! rewrites an item's file after every change to it, so the board can be
//! read with any editor, diffed and committed with git, and rebuilt from the
//! files alone (`vflt collective import --mirror`).
//!
//! ```text
//! +++
//! <item state as TOML>
//! +++
//! <spec>
//! <!-- vflt:note seq=1 actor=lead at=2026-10-11T12:00:00Z -->
//! <note body>
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{Error, Result};
use crate::types::{ActorId, ItemId, ItemState, Note};

const FENCE: &str = "+++";
const NOTE_PREFIX: &str = "<!-- vflt:note ";

pub fn path_for(dir: &Path, id: &ItemId) -> PathBuf {
    dir.join(format!("{}.md", id.as_str()))
}

pub fn render(st: &ItemState, spec: &str, notes: &[Note]) -> Result<String> {
    let mut out = String::new();
    out.push_str(FENCE);
    out.push('\n');
    out.push_str(&toml::to_string_pretty(st)?);
    out.push_str(FENCE);
    out.push('\n');
    out.push_str(spec.trim_end());
    out.push('\n');
    for n in notes {
        out.push_str(&format!(
            "\n{NOTE_PREFIX}seq={} actor={} at={} -->\n{}\n",
            n.seq,
            n.actor,
            n.at.to_rfc3339(),
            n.body.trim_end()
        ));
    }
    Ok(out)
}

pub fn parse(text: &str) -> Result<(ItemState, String, Vec<Note>)> {
    let text = text.replace("\r\n", "\n");
    let rest = text
        .strip_prefix("+++\n")
        .ok_or_else(|| Error::invalid("mirror", "missing +++ front matter"))?;
    let end = rest
        .find("\n+++\n")
        .ok_or_else(|| Error::invalid("mirror", "unterminated front matter"))?;
    let state: ItemState = toml::from_str(&rest[..end])?;
    let body = &rest[end + 5..];

    let mut spec = String::new();
    let mut notes: Vec<Note> = Vec::new();
    let mut current: Option<(Note, String)> = None;
    for line in body.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if let Some(head) = bare
            .strip_prefix(NOTE_PREFIX)
            .and_then(|h| h.strip_suffix(" -->"))
        {
            if let Some((n, b)) = current.take() {
                notes.push(finish(n, b));
            }
            current = Some((parse_head(head)?, String::new()));
        } else if let Some((_, b)) = current.as_mut() {
            b.push_str(line);
        } else {
            spec.push_str(line);
        }
    }
    if let Some((n, b)) = current.take() {
        notes.push(finish(n, b));
    }
    let mut spec = spec.trim_end().to_string();
    spec.push('\n');
    Ok((state, spec, notes))
}

fn finish(mut n: Note, body: String) -> Note {
    n.body = body.trim_end().to_string();
    n
}

fn parse_head(head: &str) -> Result<Note> {
    let mut seq = None;
    let mut actor = None;
    let mut at: Option<DateTime<Utc>> = None;
    for part in head.split_whitespace() {
        if let Some(v) = part.strip_prefix("seq=") {
            seq = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("actor=") {
            actor = Some(ActorId::new(v));
        } else if let Some(v) = part.strip_prefix("at=") {
            at = DateTime::parse_from_rfc3339(v)
                .ok()
                .map(|d| d.with_timezone(&Utc));
        }
    }
    match (seq, actor, at) {
        (Some(seq), Some(actor), Some(at)) => Ok(Note {
            seq,
            actor,
            at,
            body: String::new(),
        }),
        _ => Err(Error::invalid("mirror", format!("bad note header: {head}"))),
    }
}

/// Write `text` to `path` through a temp file and a rename.
pub fn write(path: &Path, text: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::invalid("mirror", path.display().to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    #[test]
    fn round_trips_state_spec_and_notes() {
        let now = Utc::now();
        let st = ItemState {
            id: ItemId::seq(7),
            slug: "x".into(),
            title: "A title".into(),
            stage: Stage::new("work"),
            status: Status::Pending,
            priority: 30,
            owner: ActorId::from("lead"),
            assignee: Some(ActorId::from("tester")),
            tags: vec!["area:core".into()],
            deps: vec![ItemId::seq(3)],
            created: now,
            updated: now,
            closed: None,
            external: None,
            waiting_on: None,
            raise_kind: None,
            parent: Some(ItemId::seq(2)),
            origin: None,
            bounces: 1,
            returned: true,
            summary: Some("short".into()),
            key: None,
        };
        let notes = vec![
            Note {
                seq: 1,
                actor: ActorId::from("tester"),
                at: now,
                body: "## Findings\n\nGO".into(),
            },
            Note {
                seq: 2,
                actor: ActorId::from("lead"),
                at: now,
                body: "thanks".into(),
            },
        ];
        let spec = "# Probe\n\nsteps\n\n---\n\nmore\n";
        let text = render(&st, spec, &notes).unwrap();
        let (st2, spec2, notes2) = parse(&text).unwrap();
        assert_eq!(st2.id, st.id);
        assert_eq!(st2.tags, st.tags);
        assert_eq!(st2.summary, st.summary);
        assert!(st2.returned);
        assert_eq!(spec2, spec);
        assert_eq!(notes2.len(), 2);
        assert_eq!(notes2[0].body, "## Findings\n\nGO");
        assert_eq!(notes2[1].actor.as_str(), "lead");
    }
}
