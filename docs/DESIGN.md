# vflt design

vflt is a standalone fleet orchestrator: a pool of agents and humans working
a shared board of items against a configurable source of truth. It is the
default way vpak installs large projects, but it has no dependency on vpak.

Think of each agent as a headless Claude loop with a role, a tool policy, and
a budget, donating part of a machine to a collective.

## 1. Concepts

**Collective.** A board plus its agents, profiles and policy. By default a
collective is local to one machine. Agents can be lent to another collective.

**Item.** A unit of work. The spec is a markdown document, editable while the
item is a draft and immutable after; the item accretes notes. The workflow
state is separate and mutable. Items carry a *stage* that selects which
profiles may claim them, an owner (the filer) and an assignee (who acts next).

**Agent.** A worker registered with the collective: local, remote, or human.
Has one or more profiles, usually one, and a control plane (desired state,
hold, overrides) that people and panels write and the running agent reads.

**Profile.** A role: the stages it handles, the system prompt, the runner and
model, the tool policy and classifier hints, the budget, the workspace, hooks,
batch size, and optionally a cycle.

**Runner.** The thing that executes an agent turn. Headless Claude Code is
the default. Runners are configurable per profile.

**Store.** The source of truth: git-tracked files, or sqlite (with an optional
plain-file mirror). Other backends implement the same trait later.

## 2. The state machine and the Store trait

All workflow semantics live in one pure function (`vflt-core::machine::apply`):
given the collective, an item's state, its current claim, the actor and a
transition, it updates the state and says what to persist (the event, whether
the claim goes, a note to attach). Stores only load and persist atomically, so
two stores cannot disagree about what a transition means.

```rust
pub trait Store: Send + Sync {
    fn root(&self) -> &Path;
    fn collective(&self) -> Result<Collective>;
    fn work_dir(&self) -> PathBuf;
    fn profiles_dir(&self) -> PathBuf;

    // items
    fn create_item(&self, spec: &str, meta: NewItem) -> Result<Item>;
    fn get_item(&self, id: &ItemId) -> Result<Item>;            // with spec
    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>>; // without specs
    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>; // atomic
    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()>;
    fn break_claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;
    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item>;
    fn update_item(&self, id: &ItemId, by: &ActorId, u: ItemUpdate) -> Result<Item>;
    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note>;
    fn notes(&self, id: &ItemId) -> Result<Vec<Note>>;
    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item>;
    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item>;

    // agents
    fn register_agent(&self, a: NewAgent) -> Result<Agent>;
    fn get_agent(&self, id: &ActorId) -> Result<Agent>;
    fn update_agent(&self, cfg: &AgentConfig, by: &ActorId, summary: &str) -> Result<()>;
    fn set_runtime(&self, id: &ActorId, rt: &AgentRuntime) -> Result<()>;
    fn heartbeat(&self, id: &ActorId) -> Result<()>;
    fn last_heartbeat(&self, id: &ActorId) -> Result<Option<DateTime<Utc>>>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn deregister_agent(&self, id: &ActorId) -> Result<()>;

    // events, bundles
    fn append_event(&self, e: Event) -> Result<()>;
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;
    fn import_record(&self, rec: BundleRecord) -> Result<()>;
    fn export_records(&self) -> Result<Vec<BundleRecord>>;
    fn rebuild_mirror(&self) -> Result<usize>;     // stores with a mirror
}
```

Profiles are plain files in every store (`profiles/<name>.{toml,md}`), loaded
by the trait's default methods with `#include` lines expanded.

## 3. Stores

`collective.toml`:

```toml
ids = "ulid"            # ulid | seq
[store]
kind = "file"           # file | sqlite | turso | dynamodb | mysql | http
root = "."
git  = true             # file: commit state changes
url  = "board.db"       # sqlite: the database file
mirror = "board"        # sqlite: plain-file mirror directory (optional)
```

### File store

```
<collective-root>/
├── collective.toml
├── items/<id>/
│   ├── spec.md          # immutable once the item leaves draft
│   ├── state.toml       # stage, status, priority, owner, assignee, tags, deps,
│   │                    # parent, origin, bounces, returned, summary, key, ...
│   ├── notes/           # NNNN-<actor>.md, append-only
│   ├── events.jsonl     # per-item event log
│   └── .claim           # exists while claimed; created with O_EXCL
├── agents/<id>.toml     # identity + control plane (tracked)
├── agents/<id>.hb       # heartbeat (not tracked)
├── agents/<id>.rt.json  # runtime status (not tracked)
├── profiles/<name>.{toml,md}
└── events.jsonl         # collective-wide event log
```

Claims use `create_new` on `.claim`, atomic on POSIX and Windows. Sequential
ids race through `create_dir` on the item directory, which exactly one creator
wins. With `git = true` every state mutation is followed by `git add` of the
touched paths and a commit whose message is the event summary. Multi-machine
collectives need a database or http backend.

### Sqlite store

One database holds items (state as JSON, the source of truth, next to indexed
copies of the fields queries filter on), claims, notes, events, agents,
runtime and heartbeats. WAL mode, a generous busy timeout, and every write in
an immediate transaction: a claim is atomic across threads and processes, and
a transition lands with its note and event or not at all.

With `mirror` set, every change to an item rewrites `<mirror>/<id>.md`:

```
+++
<item state as TOML>
+++
<spec>
<!-- vflt:note seq=1 actor=lead at=2026-10-11T12:00:00Z -->
<note body>
```

The mirror is for people and git; the database stays the source of truth, and
mirror trouble never fails the operation that caused it. `collective import
--mirror` rebuilds a board from the files alone.

### Bundles

`collective export` writes JSON lines, one `BundleRecord` each (`item` with
its spec, `note`, `event`, `agent`), items first. `collective import` writes
them back as-is: ids, timestamps, notes and trails survive a move between
stores, and any tracker that can produce the shape can be imported.

## 4. Item lifecycle

```toml
# items/<id>/state.toml (file store) — the same fields as JSON in sqlite
id        = "vf-01j9k3..."      # or "vf-42" with ids = "seq"
slug      = "payments-rds-module"
title     = "Write RDS Postgres module for payments DB"
stage     = "code"
status    = "pending"           # draft | pending | claimed | in_progress | blocked
                                # | needs_human | done | cancelled
priority  = 50                  # lower runs first
owner     = "supervisor"        # who filed it; never changes
assignee  = ""                  # agent id, human id, profile name, or empty (pool)
tags      = ["db", "aws"]
deps      = ["vf-01j9k2..."]    # must be done or cancelled before this is claimable
parent    = "vf-01j9k1..."      # the item this follows up on
origin    = { stage = "code", assignee = "coder" }   # where a bounce returns it
bounces   = 1
returned  = true                # came back by a bounce; cleared when picked up
summary   = "..."               # plain-language summary for people
key       = "disk_low"          # flag key: at most one open item carries it
raise_kind = "decision"         # while needs_human: decision | action
waiting_on = "..."              # while blocked / needs_human: the reason
created   = "2026-10-02T21:10:00Z"
updated   = "2026-10-02T21:12:00Z"
closed    = "..."               # when it reached done or cancelled

[external]                      # optional, reserved for ticket sync
system = "jira"
key    = "PAY-123"
```

| From                     | Verb        | To                                  | Notes |
|--------------------------|-------------|-------------------------------------|-------|
| draft                    | promote     | pending                             | spec freezes |
| pending (unclaimed)      | demote      | draft                               | |
| pending                  | claim       | claimed                             | atomic |
| claimed, pending         | start       | in_progress                         | claims first from pending |
| in_progress, claimed     | complete    | next stage pending, or done         | records origin = previous stage + resting assignee |
| in_progress, claimed     | complete --bounce | origin (or bounce_to), pending | marks returned, counts bounces |
| pending, in_progress     | wait        | pending + deps                      | cycle-checked |
| pending, in_progress     | block       | blocked                             | reason in waiting_on |
| any open                 | raise       | needs_human                         | question + kind |
| any open                 | handoff     | pending, new assignee               | records origin; a draft stays draft |
| blocked, needs_human     | reopen      | pending                             | optionally reassigns |
| any open                 | cancel      | cancelled                           | |

Every transition writes an event whose detail carries `from_status` and
`to_status` (and `note_seq` when it attached a note). The *resting assignee*
is the assignee an item had before it was claimed: release, wait and the
origin record use it, so a claim by one agent of a pool never sticks the item
to that agent.

A delegated item (filed with an assignee other than the filer) records the
filer as its origin, so a bounce returns it to whoever asked.

`respond` is a CLI composition: a note, then reopen (raised or blocked item)
or handoff to the owner (a pending item resting with the responder).

## 5. Profiles

```toml
name        = "coder"
description = "Implements items in the code stage"
stages      = ["code"]
runner      = "claude"
model       = ""
cycle       = ""                 # empty: wake on work. "2h": also cycle turns when idle
batch       = 1

[policy]
permission_mode  = "auto"
allowed_tools    = ["Read", "Edit", "Write", "Grep", "Glob", "Bash(git *)", "Bash(vflt *)"]
disallowed_tools = ["Bash(rm -rf *)"]

[policy.classifier]
environment = ["Workspace is the item's checkout under the collective root."]
allow       = ["Running the project's own build and test commands."]
soft_deny   = ["Pushing to any remote branch other than the item's branch."]
hard_deny   = ["Deleting files outside the workspace.", "Reading or printing credential files."]

[budget]
max_turns  = 60
wall_clock = "45m"
daily_usd  = 20.0

[workspace]
mode     = "item"                # item | shared | worktree
dir      = ""
add_dirs = []

[hooks]
gate   = []
before = []
after  = []

[env]
```

`profiles/coder.md` is the system prompt; `#include <path>` lines are expanded
relative to the collective root, recursively, every time the profile loads.
At launch it is followed by the vflt primitive guide (the exact commands to
finish each item held, or the cycle-turn instructions).

The **supervisor** profile has no stages and a cycle. A profile with stages
*and* a cycle is a **hybrid**: routed work first; a cycle turn only when
nothing is claimable, the cycle has elapsed, the agent is not held, and it is
not snoozed (or a wake was requested).

Classifier rules are advisory text handed to the runner's permission
classifier. Headless runs cannot prompt, so an action the classifier would ask
about is denied, and the denial is recorded on the item as a note.

## 6. Runner

```rust
pub trait Runner: Send + Sync {
    fn name(&self) -> &str;
    fn available(&self) -> Result<Availability>;
    fn run(&self, req: RunRequest) -> Result<RunOutcome>;
}
```

`RunRequest` carries the prompts, the workspace (`cwd`), a private scratch
directory for prompt files, extra directories, the model, policy, budget and
environment. Every launch gets `VFLT_COLLECTIVE`, `VFLT_AGENT`,
`VFLT_PROFILE`, `VFLT_ITEM`, `VFLT_ITEMS`, `VFLT_WORKSPACE` and `VFLT_BIN`
(the exact binary running the loop), so an agent's `vflt item ...` calls hit
the same build. Children run in their own process group: an interrupt aimed at
the orchestrator does not kill a turn.

## 7. Agent loop

`vflt agent run --profile coder [--once] [--interval 15s] [--name <name>]`,
or `vflt fleet run` for every registered local agent, a thread each.

1. Register the agent (or resume by name). Release any item it still holds
   from a turn that never finished, with a note.
2. Each pass: heartbeat; re-read the agent's control plane and its profile.
   `stopped` → exit; `paused` → idle.
3. Select up to `batch` claimable items: `pending`, unclaimed, stage handled
   by the profile, every dependency done or cancelled, assignee empty / the
   agent / the profile name (by claim mode). Under a hold, skip items the
   agent filed. Lowest priority number first, then oldest.
4. Nothing selected: run a cycle turn if one is due, else idle.
5. Something to do: run the `gate` hooks and check the daily spend cap; a
   failure holds the agent (`gated`) without claiming.
6. Claim and start the batch; work routed by someone else clears a snooze.
   Prepare the workspace, run `before` hooks, launch the runner with a
   heartbeat thread alongside, run `after` hooks.
7. Record an `agent.run` event per item (exit, turns, cost share,
   transcript). Any item the turn left in progress is raised with the
   transcript tail.

Runtime status (`active`, `idle`, `paused`, `gated`, `offline`), the current
items and a one-line note are written as the agent goes, for `agent list`
and panels.

**Interrupts.** The first SIGINT/SIGTERM/SIGHUP asks every loop in the
process to finish its turn and exit, leaving desired states alone; a second
aborts the running turns.

## 8. Control plane

```
vflt agent pause|resume|stop <name>... | --all
vflt agent wake <name>                  # one cycle turn at the next opportunity
vflt agent hold <name> on|off           # routed work only
vflt agent snooze [<name>]              # no cycle turns until work is routed here
vflt agent set <name> --model M --cycle 2h --wall-clock 45m --daily-usd 10   # "none" clears
vflt agent list                         # + queue counts and a one-word reading
```

`AgentConfig` (identity + control plane) is tracked and written through
`update_agent` with an event; `AgentRuntime` (status, current items, note,
last cycle, snooze) is untracked and written only by the running agent.

## 9. The human side

```
vflt inbox [--kind decision|action]     # raised items + attention
vflt item respond <id> --note "..."     # answer and route on
vflt flag --key K --title T [--body B]  # one item per fleet-wide fact
vflt cost [--since 7d] [--by agent|day|item]
```

Attention kinds: `stale_claim`, `stranded` (assigned to nobody who can take
it), `stalled_chain` (waiting on a blocked or raised item), `long_blocked`,
`stale_draft`, `paused_backlog` (ready work behind a paused or stopped agent),
`idle_backlog` (ready work for an agent that is not running), `with_person`
(pending work handed to a person a day ago and never raised).

## 10. Collectives and lending

```
vflt collective init [<dir>] [--name <n>] [--store file|sqlite] [--ids ulid|seq]
                     [--mirror <dir>] [--claim-mode pull|delegated|both] [--pipeline a,b,done]
vflt collective show | export | import
vflt agent add --name <n> --profile <p> [--remote <url> | --human]
vflt agent lend <agent> --to <collective-root|url>
```

Lending to a remote collective requires that collective's store to be
reachable from this machine (an http store). In this release lending works to
another collective on the same filesystem.

A `.vflt` *file* (`path = "<dir>"`) at a repository root points discovery at
a collective kept in a subdirectory.

## 11. Ticket sync

Out of scope for now, but the schema reserves `[external]` and the CLI has
`item link`. A sync adapter will read and write that field.

## 12. Cross-platform

Both tools target macOS, Linux and Windows. Rules that follow from that:

- **No `/dev/tty`.** Interactive prompts go through a cross-platform console
  abstraction. Fall back to stdin when no console is available.
- **No symlinks in data layouts.** Where the layout points at another
  directory, write a small toml file with a `path` key instead (the `.vflt`
  pointer).
- **No `sh -c`.** Runner and hook command templates are split into argv
  (shlex-style) and spawned directly. Anyone who wants a shell names it.
- **Identity** comes from `USER`, then `USERNAME`, then the `whoami` crate.
- **Paths** are `PathBuf` end to end.
- **Config and data dirs** come from the `dirs` crate.
- **Executable lookup** goes through `which`, which honours `PATHEXT`.
- **Atomic claims** use `create_new` (file store) or an immediate sqlite
  transaction.
- **Budgets** kill the child process on wall-clock expiry; interrupts use the
  `ctrlc` crate; children get their own process group (Unix) or a new process
  group (Windows).
- **Line endings**: write `\n`, read tolerant of `\r\n`.
- **Test fixtures** are small Rust binaries, not shell scripts.
- **CI** runs `cargo build`, `cargo test`, `cargo clippy` and `cargo fmt
  --check` on an ubuntu / macos / windows matrix.

## 13. Crate layout

```
vflt/
├── Cargo.toml                 # workspace
├── crates/
│   ├── vflt-core/             # types, machine, Store trait, file + sqlite stores,
│   │                          # mirror, profiles, selection
│   ├── vflt-runner/           # Runner trait, claude runner, shell runner, process
│   └── vflt/                  # CLI binary: commands, agent loop, fleet
├── profiles/                  # default profiles copied into a new collective
├── docs/DESIGN.md
└── README.md
```
