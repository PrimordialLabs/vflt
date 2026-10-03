# vflt design

vflt is a standalone fleet orchestrator: a pool of agents and humans working
a shared board of items against a configurable source of truth. It is the
default way vpak installs large projects, but it has no dependency on vpak.

Think of each agent as a headless Claude loop with a role, a tool policy, and
a budget, donating part of a machine to a collective.

## 1. Concepts

**Collective.** A board plus its agents, profiles and policy. By default a
collective is local to one machine. Agents can be lent to a remote
collective.

**Item.** A unit of work. The spec is an immutable markdown file that
accretes notes; the workflow state is separate and mutable. Items carry a
*stage* that selects which profiles may claim them.

**Agent.** A worker registered with the collective. Local or remote. Has one
or more profiles, usually one. Humans are agents too: a person can claim an
item by assigning it to themselves.

**Profile.** A role: the stages it handles, the system prompt, the runner,
the tool policy and classifier hints, the budget, and optionally a cycle.
Defaults ship for supervisor, surveyor, planner, coder, reviewer, tester,
deployer.

**Runner.** The thing that executes an agent turn. Headless Claude Code is
the default. Runners are configurable per profile.

**Store.** The source of truth. Default is git-tracked files on disk. Other
backends (sqlite, turso, dynamodb, mysql) implement the same trait later.

## 2. Store trait

```rust
pub trait Store: Send + Sync {
    fn collective(&self) -> Result<Collective>;

    // items
    fn create_item(&self, spec: &str, meta: NewItem) -> Result<Item>;
    fn get_item(&self, id: &ItemId) -> Result<Item>;
    fn list_items(&self, filter: &ItemFilter) -> Result<Vec<Item>>;
    fn claim(&self, id: &ItemId, by: &ActorId) -> Result<Claim>;        // atomic
    fn release(&self, id: &ItemId, by: &ActorId) -> Result<()>;
    fn transition(&self, id: &ItemId, by: &ActorId, to: Transition) -> Result<Item>;
    fn add_note(&self, id: &ItemId, by: &ActorId, body: &str) -> Result<Note>;
    fn set_assignee(&self, id: &ItemId, by: &ActorId, to: Option<ActorId>) -> Result<Item>;
    fn link_external(&self, id: &ItemId, by: &ActorId, ext: ExternalRef) -> Result<Item>;

    // agents
    fn register_agent(&self, a: NewAgent) -> Result<Agent>;
    fn heartbeat(&self, id: &ActorId) -> Result<()>;
    fn list_agents(&self) -> Result<Vec<Agent>>;
    fn deregister_agent(&self, id: &ActorId) -> Result<()>;

    // profiles & events
    fn profiles(&self) -> Result<Vec<Profile>>;
    fn append_event(&self, e: Event) -> Result<()>;
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;
}
```

Store selection comes from `collective.toml`:

```toml
[store]
kind = "file"          # file | sqlite | turso | dynamodb | mysql | http
root = "."             # for file
git  = true            # commit state changes
```

Only `file` is implemented in the first cut. The others parse and return a
clear "not implemented" error so the configuration surface is stable.

## 3. File store layout

```
<collective-root>/
├── collective.toml
├── items/
│   └── <id>/
│       ├── spec.md          # immutable after creation
│       ├── state.toml       # stage, status, priority, owner, assignee, tags, deps, external
│       ├── notes/           # NNNN-<actor>.md, append-only
│       ├── events.jsonl     # per-item event log
│       └── .claim           # exists while claimed; created with O_EXCL; holds actor + ts
├── agents/
│   └── <id>.toml            # name, profiles, kind, host, status, last_seen, lent_to
├── profiles/
│   ├── <name>.toml          # stages, runner, policy, budget, cycle
│   └── <name>.md            # system prompt
└── events.jsonl             # collective-wide event log
```

Claims use `create_new` on `.claim`, which is atomic on POSIX filesystems. A
stale claim (holder has no heartbeat within `claim_ttl`) can be broken by
`vflt item release --force`, and the break is an event.

When `store.git = true`, every state mutation is followed by `git add` of the
touched paths and a commit whose message is the event summary. Spec files are
committed once at creation. Notes are committed as they land.

Multi-machine collectives on the file store are not supported; that needs a
database or http backend. The CLI refuses `agent lend` to a file-store
collective on another host.

## 4. Item lifecycle

```toml
# items/<id>/state.toml
id        = "vf-01j9k3..."
slug      = "payments-rds-module"
title     = "Write RDS Postgres module for payments DB"
stage     = "code"            # survey | plan | code | review | test | deploy | done (custom allowed)
status    = "pending"         # pending | claimed | in_progress | blocked | needs_human | done | cancelled
priority  = 50                # lower runs first
owner     = "supervisor"      # who filed it
assignee  = ""                # agent id, human id, or empty for the pool
tags      = ["db", "aws"]
deps      = ["vf-01j9k2..."]  # must be done before this is claimable
created   = "2026-10-02T21:10:00Z"
updated   = "2026-10-02T21:12:00Z"

[external]                    # optional, reserved for ticket sync
system = "jira"
key    = "PAY-123"
url    = "https://..."
```

Transitions:

| From                | Verb        | To            | Notes                                        |
|---------------------|-------------|---------------|----------------------------------------------|
| pending             | claim       | claimed       | atomic; stage must be in claimant's profile  |
| claimed             | start       | in_progress   | runner launched                              |
| in_progress         | complete    | next stage, pending | reviewer bounce goes back to `code`    |
| in_progress         | block       | blocked       | hard dependency; records what it waits on    |
| in_progress         | raise       | needs_human   | question recorded; humans drain this         |
| any                 | handoff     | pending       | assignee changed, stage optionally changed   |
| blocked/needs_human | reopen      | pending       | by a human or supervisor                     |
| any                 | cancel      | cancelled     |                                              |

Default stage pipeline: `survey -> plan -> code -> review -> test -> deploy -> done`.
A reviewer completing with `--bounce` sends the item back to `code`. The
pipeline is configurable per collective.

## 5. Profiles

```toml
# profiles/coder.toml
name        = "coder"
description = "Implements items in the code stage"
stages      = ["code"]
runner      = "claude"
cycle       = ""                 # empty: wake on work. "15m": run every 15 minutes regardless.

[policy]
permission_mode  = "auto"        # auto | acceptEdits | plan | dontAsk | bypassPermissions
allowed_tools    = ["Read", "Edit", "Write", "Grep", "Glob", "Bash(git *)", "Bash(vflt *)", "Bash(cargo *)"]
disallowed_tools = ["Bash(rm -rf *)"]

# Advisory rules for the runner's permission classifier. The claude runner
# maps these onto Claude Code's `autoMode` settings block, passed inline
# with --settings for the run. Other runners map them however they can.
[policy.classifier]
environment = ["Workspace is the item's checkout under the collective root."]
allow       = ["Running the project's own build and test commands."]
soft_deny   = ["Pushing to any remote branch other than the item's branch."]
hard_deny   = ["Deleting files outside the workspace.", "Reading or printing credential files."]

[budget]
max_turns  = 60
wall_clock = "45m"
```

`profiles/coder.md` is the system prompt. It receives, at launch, the item
spec, the collective context, and the vflt primitive guide (how to `note`,
`complete`, `block`, `raise`).

The **supervisor** profile has no stages and a cycle. Each cycle it reads the
board and files, reprioritizes or closes items. vpak uses it as the handoff
point: the supervisor's standing instructions are the install's compiled
bootstrap.

Classifier rules are advisory text handed to the runner's permission
classifier. Headless runs cannot prompt, so an action the classifier would
ask about is denied and the denial is recorded on the item as a `needs_human`
question. They are sensible defaults and a vpak install customizes them
during its bootstrap.

## 6. Runner

```rust
pub trait Runner: Send + Sync {
    fn name(&self) -> &str;
    fn available(&self) -> Result<Availability>;
    fn run(&self, req: RunRequest) -> Result<RunOutcome>;
}
```

Runners register by name. `claude` is the default and shells out to
`claude -p` with the profile policy. A `shell` runner runs a command template
with the prompt on stdin and is used for tests. Any agent harness that can be
launched as a process and told what to do can be a runner.

## 7. Agent loop

`vflt agent run --profile coder [--once] [--interval 15s] [--name <name>]`

1. Register the agent (or resume by name). Heartbeat.
2. **Pull mode** (default): pick the highest priority `pending` item whose
   stage is in the profile, whose deps are all done, and whose assignee is
   empty or this agent. **Delegated mode**: only items assigned to this
   agent. The collective sets `claim_mode = pull | delegated | both`.
3. Claim. Start. Prepare a workspace directory for the item.
4. Launch the runner with the profile prompt, the item spec, and the guide.
5. The runner is expected to finish by calling `vflt item complete|block|raise`.
   If it exits without one, the loop records the outcome from the exit and
   marks the item `needs_human` with the transcript tail as a note.
6. Release claim. Loop. Idle agents poll at `--interval`; a cycle profile
   runs on its cycle instead.

Humans: `vflt item claim <id> --me` and `vflt item complete <id> --me`.

## 8. Collectives and lending

```
vflt collective init [<dir>] [--name <n>]
vflt collective show
vflt agent add --name <n> --profile <p> [--remote <url>]    # register only
vflt agent run --profile <p>                                  # register and work
vflt agent lend <agent> --to <collective-root|url>            # point a local agent at another board
vflt agent list
```

Lending to a remote collective requires that collective's store to be
reachable from this machine (database or http). In the first cut lending
works to another file-store collective on the same filesystem, which is
enough to exercise the mechanism.

## 9. Items CLI

```
vflt item add --title <t> [--stage <s>] [--priority <n>] [--assign <actor>] [--dep <id>]... [--tag <t>]... [--spec-file <f> | -]
vflt item list [--stage s] [--status s] [--assignee a] [--mine] [--json]
vflt item show <id>
vflt item claim <id> [--me | --as <agent>]
vflt item start <id>
vflt item note <id> [--body-file <f> | -]
vflt item complete <id> [--bounce] [--to-stage <s>] [--note <t>]
vflt item block <id> --on <text>
vflt item raise <id> --question <text>
vflt item handoff <id> --to <actor> [--stage <s>]
vflt item reopen <id>
vflt item cancel <id>
vflt item release <id> [--force]
vflt item link <id> --system jira --key PAY-123 [--url <u>]
vflt item assign <id> <actor>
vflt events [--item <id>] [--since <ts>] [--json]
vflt board                                           # one-screen summary
```

All commands accept `--json` for machine use. `--collective <dir>` or
`VFLT_COLLECTIVE` selects the collective; default is the nearest ancestor
directory containing `collective.toml`, then `./.vflt`.

## 10. Ticket sync

Out of scope for the first cut, but the schema reserves `[external]` and the
CLI has `item link`. A sync adapter will read and write that field.

## 11. Cross-platform

Both tools target macOS, Linux and Windows. Rules that follow from that:

- **No `/dev/tty`.** Interactive prompts go through a cross-platform console
  abstraction (the `console` / `dialoguer` crates, which open `CONIN$` on
  Windows). Fall back to stdin when no console is available.
- **No symlinks in data layouts.** Where the layout pointed at another
  directory, write a small toml file with a `path` key instead.
- **No `sh -c`.** Runner command templates are split into argv (shlex-style)
  and spawned directly. Anyone who wants a shell names it in the template.
- **Identity** comes from `USER`, then `USERNAME`, then the `whoami` crate.
- **Paths** are `PathBuf` end to end. Archive entries always use `/`.
  Windows verbatim prefixes are stripped before writing paths into files.
- **Config and data dirs** come from the `dirs` crate, never a hand-built
  `~/.config`.
- **Executable lookup** goes through `which`, which honours `PATHEXT`.
- **Atomic claims** use `create_new`, which is atomic on all three platforms.
- **Budgets** kill the child process on wall-clock expiry; no signals.
- **Line endings**: write `\n`, read tolerant of `\r\n`.
- **Test fixtures** are small Rust binaries, not shell scripts. Any fixture
  that must be a script is `#[cfg(unix)]` gated and has a Windows twin or a
  documented skip.
- **CI** runs `cargo build`, `cargo test` and `cargo clippy` on an
  ubuntu / macos / windows matrix (`.github/workflows/ci.yml`).

## 12. Crate layout

```
vflt/
├── Cargo.toml                 # workspace
├── crates/
│   ├── vflt-core/             # types, Store trait, FileStore, profiles, policy, selection
│   ├── vflt-runner/           # Runner trait, claude runner, shell runner
│   └── vflt/                  # CLI binary
├── profiles/                  # default profiles copied into a new collective
├── docs/DESIGN.md
└── README.md
```
