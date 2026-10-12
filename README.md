# vflt

vflt is a standalone fleet orchestrator: a pool of agents and humans working a
shared board of items against a configurable source of truth. Each agent is a
headless agent loop with a role (a *profile*), a tool policy, and a budget,
donating part of a machine to a *collective*. Humans sit on the same board and
can take any item themselves.

It is the default way [vpak](../vpak) installs large projects, but it has no
dependency on vpak.

Design: [docs/DESIGN.md](docs/DESIGN.md).

## Install

Straight from GitHub, no clone needed:

```sh
cargo install --git https://github.com/primordiallabs/vflt vflt
```

Or from a checkout of this repo:

```sh
cargo install --path crates/vflt
```

Either way the `vflt` binary lands in `~/.cargo/bin`. Requires Rust 1.85+
and, for the git-tracked file store, `git` on PATH. Builds and tests on macOS,
Linux and Windows.

## Quickstart

```sh
# 1. a collective: a board plus profiles, as git-tracked files in ./.vflt
vflt collective init --name payments
#    or a sqlite board for a busy fleet, with short ids (vf-1, vf-2, ...)
#    and a plain-file mirror of every item for git:
vflt collective init --name payments --store sqlite --ids seq --mirror board

# 2. items: a spec plus workflow state and notes
vflt item add --title "Survey the target AWS account" --stage survey --spec-file survey.md
vflt item add --title "RDS module for payments DB" --stage code --spec "Write the Terraform module..." --priority 20
vflt board

# 3. an agent working the board with the default headless Claude Code runner
vflt agent run --profile coder
#    or every registered agent at once
vflt agent add --name coder-1 --profile coder
vflt agent add --name reviewer-1 --profile reviewer
vflt fleet run

# 4. or take an item yourself
vflt item claim vf-01m3z --me        # unique id prefixes are fine
vflt item start vf-01m3z
vflt item note vf-01m3z --body "started; using the existing VPC module"
vflt item complete vf-01m3z --note "module written, plan is clean"

# 5. watch and steer
vflt item list
vflt inbox                           # what needs a human, what looks stuck
vflt agent list                      # who is doing what, queue sizes
vflt events --since 2h
vflt cost --since 7d --by agent
```

Every command takes `--json`. The collective is found from `--collective`,
`VFLT_COLLECTIVE`, the nearest ancestor holding `collective.toml` (directly,
under `.vflt/`, or through a `.vflt` pointer file — see below), then `./.vflt`.

## How work flows

Items carry a **stage**. The default pipeline is

```
survey -> plan -> code -> review -> test -> deploy -> done
```

and `vflt item complete` advances an item one stage. Profiles declare which
stages they handle, so a `coder` agent only ever claims `code` items. Items can
depend on other items (`--dep`, or `item wait` after the fact), follow up on a
parent (`--parent`), be delegated to an agent, a human or a whole profile
(`--assign coder`), and carry an external ticket reference (`item link`).

A collective that routes purely by assignee (`--claim-mode delegated
--pipeline work,done`) works as a set of lanes: every agent handles the one
working stage and takes only what is routed to it.

| Verb      | From                         | To                                   |
|-----------|------------------------------|--------------------------------------|
| promote   | draft                        | pending                              |
| demote    | pending (unclaimed)          | draft                                |
| claim     | pending                      | claimed                              |
| start     | claimed or pending           | in_progress                          |
| complete  | in_progress                  | next stage (pending), or done        |
| complete --bounce | in_progress          | back where it came from (pending, marked returned) |
| wait      | pending or in_progress       | pending until its new deps are done  |
| block     | pending or in_progress       | blocked                              |
| raise     | any open                     | needs_human (decision or action)     |
| handoff   | any open                     | pending, new assignee (a draft stays a draft) |
| reopen    | blocked, needs_human         | pending                              |
| cancel    | any open                     | cancelled                            |

**Bounces go back to the origin.** When an item moves forward (complete to the
next stage, or a handoff) it records where it came from: the previous stage
and assignee. When a delegated item is filed (an assignee other than the
filer), the filer is its origin. `complete --bounce` returns it there, so a
reviewer's rejection lands with the coder pool that wrote it, and a failed
check lands with whoever asked for it. With no origin it goes to the
collective's `bounce_to` stage.

**Drafts** are filed but not claimable: visible on the board, spec still
editable (`item set --spec-file`), released with `promote`. Once an item
leaves draft its spec is immutable; everything learned goes in notes.

**Raising** asks a human for a `decision` (a call to make) or an `action`
(something only a person can do). `vflt inbox` lists them, plus what looks
stuck: stale claims, items assigned to no one who can take them, chains
waiting on blocked items, long blocks, old drafts, work handed to a person
and never raised, and backlogs behind paused or idle agents. `vflt item respond <id> --note "..."` answers: the note lands,
a raised or blocked item reopens for its assignee, and an item resting with
the responder goes back to its owner.

**Flags** are fleet-wide facts ("the build is broken", "the disk is full")
that many agents may notice at once: `vflt flag --key disk_low --title ...`
files one raised item, and later flags with the same key add a note to it
instead of filing duplicates. Closing the item re-arms the key.

Claims are atomic. A claim whose holder has stopped heartbeating goes stale
after `claim_ttl` and can be released by anyone; `item release --force`
breaks any claim and records that it did. An agent that starts releases any
item it still holds from a turn that never finished.

## Profiles

A profile is a role: `profiles/<name>.toml` plus `profiles/<name>.md` (the
system prompt). Seven ship by default and are copied into every new
collective:

| profile    | stages  | notes                                            |
|------------|---------|--------------------------------------------------|
| supervisor | (cycle) | runs every 15m; files, reprioritises, escalates   |
| surveyor   | survey  | read-only discovery, records facts with sources   |
| planner    | plan    | turns findings into small items                   |
| coder      | code    | implements inside the item workspace              |
| reviewer   | review  | verifies against the spec; `complete --bounce`    |
| tester     | test    | exercises end to end in disposable resources      |
| deployer   | deploy  | applies exactly the steps in the spec             |

Edit them in place; they are plain files, re-read before every turn. A
profile prompt may pull in shared text with `#include <path>` lines (relative
to the collective root), so a team keeps its common rules in one file.

```toml
name    = "coder"
stages  = ["code"]
runner  = "claude"
model   = ""         # runner's default; agents can override
cycle   = ""         # "2h": also run a cycle turn when idle (see below)
batch   = 1          # items per turn

[policy]
permission_mode  = "auto"
allowed_tools    = ["Read", "Edit", "Bash(git *)", "Bash(vflt *)"]
disallowed_tools = ["Bash(rm -rf *)"]
mcp_config       = ["mcp/base.json"]   # only these MCP servers (unset: runner default)

[policy.classifier]       # advisory rules for the runner's permission classifier
environment = ["Workspace is the item's checkout under the collective root."]
allow       = ["Running the project's own build and test commands."]
soft_deny   = ["Pushing to any remote branch other than the item's branch."]
hard_deny   = ["Deleting files outside the workspace."]

[budget]
max_turns  = 60
wall_clock = "45m"
daily_usd  = 20.0        # optional per-agent daily spend cap

[workspace]
mode     = "item"        # item | shared | worktree
dir      = "."           # for shared and worktree, relative to the collective root
add_dirs = []

[hooks]
gate   = ["scripts/check-disk"]   # must pass before the agent claims anything
before = []                       # after the claim, before the runner
after  = ["scripts/lint-notes"]   # after the runner; failures become notes

[env]
EXTRA = "value"                   # for runners and hooks
```

**Workspaces.** `item` (default) gives each item a private directory under
`work/`. `shared` runs every turn in one existing directory, such as a
repository checkout several agents work together. `worktree` gives each item
its own git worktree of `dir`, on branch `vflt/<item>`.

**Hooks** are command templates, split shlex-style and run directly with the
`VFLT_*` environment. A failing `gate` holds the agent (status `gated`, the
first output line as its note) until a later check passes; a failing `before`
hook releases the turn's items and holds the agent the same way; a failing
`after` hook adds its output to each item of the turn as a note.

**Batches.** With `batch = N` an agent claims up to N items into one turn,
which is the right shape when a turn has expensive setup (a server, a
browser) worth sharing.

**Cycle turns.** A profile with no stages and a `cycle` runs on its cycle (the
supervisor). A profile with stages *and* a cycle is a hybrid: it works what is
routed to it, and when nothing is claimable and the cycle has elapsed it runs
a cycle turn with no item, to look at the board and file its next piece of
work. An agent that is caught up runs `vflt agent snooze`; cycles resume when
someone else routes work to it.

## Agents and the control plane

`vflt agent add --name <n> --profile <p>` registers an agent; `--human`
registers a person. Each agent has a control plane that anyone (or any panel)
can write and the running agent reads between turns:

```sh
vflt agent pause coder-1          # take no new work (resume to continue)
vflt agent stop --all             # finish the current turn, then exit
vflt agent wake planner-1         # run a cycle turn at the next opportunity
vflt agent hold coder-1 on        # only work others route here: no cycles, no self-filed items
vflt agent set coder-1 --model <m> --cycle 2h --wall-clock 60m --daily-usd 10
vflt agent list                   # desired state, what it is doing, its queue
```

The tracked part of an agent (identity, control plane) is stored apart from
its runtime status, which only the running agent writes. A running agent
heartbeats throughout every turn, so a long turn is never mistaken for a dead
one.

`vflt fleet run` runs every registered local agent in one process, a thread
each. Interrupt it (Ctrl-C, SIGTERM) and every agent finishes its current turn
and exits, with its control plane untouched; interrupt again to abort the
running turns. `--max-parallel N` shares N turn slots across the fleet: an
agent with work takes a slot before it claims anything and reads `waiting`
until one frees, so a fleet larger than its machine takes turns.

## Runners

A runner executes one agent turn as a child process in its own process group.
The agent is told, in its system prompt, exactly which `vflt item ...`
command finishes each item it holds. If it exits with an item still in
progress, the loop raises the item with the transcript tail as a note.

- **`claude`** (default): headless Claude Code. `claude -p --permission-mode
  <mode> --permission-prompts none --allowedTools ... --max-turns N
  --output-format stream-json [--model M]`, with the classifier rules passed
  as the `autoMode` settings block. Transcripts land in `work/<item>/runs/`.
- **`shell`**: any command template, split shlex-style and spawned directly
  (never through a shell). Set `runner = "shell"` and `runner_command = "..."`
  in the profile. The prompt arrives on stdin; `VFLT_PROMPT_FILE`,
  `VFLT_SYSTEM_PROMPT_FILE`, `VFLT_COLLECTIVE`, `VFLT_ITEM`, `VFLT_ITEMS`,
  `VFLT_WORKSPACE`, `VFLT_AGENT` and `VFLT_MODEL` are set.
- **custom**: name a command template once in the user config and refer to it
  from any profile with `runner = "codex"`:

```toml
# <config dir>/vflt/config.toml   (~/.config/vflt on Linux, ~/Library/Application Support/vflt on macOS, %APPDATA%\vflt on Windows)
[runners.codex]
command = "codex exec --full-auto"
```

Prompt files go to a private scratch directory per turn, never the
workspace. Wall-clock budgets are enforced by killing the child. Runner cost
is recorded on every run; `vflt cost` sums it by agent, day or item.

## Store

`collective.toml` names the source of truth:

```toml
[store]
kind = "file"   # file | sqlite | turso | dynamodb | mysql | http
root = "."
git  = true     # file store: commit every board mutation
```

- **`file`**: items, agents, profiles and events are plain files under the
  collective root, and with `git = true` every state change is a commit whose
  message is the event.

  ```
  .vflt/
  ├── collective.toml
  ├── items/<id>/{spec.md, state.toml, notes/, events.jsonl, .claim}
  ├── agents/<id>.toml          (+ <id>.hb heartbeat and <id>.rt.json runtime, not tracked)
  ├── profiles/<name>.{toml,md}
  ├── events.jsonl
  └── work/<id>/                (per-item workspaces and runner transcripts, not tracked)
  ```

- **`sqlite`**: the whole board in one database (`url`, default `board.db`),
  every write an immediate transaction, so claims stay atomic across threads
  and processes at any board size. Set `mirror = "board"` to keep a plain-file
  mirror: one markdown file per item (state, spec, notes), rewritten on every
  change, ready to diff, grep and commit, and importable back
  (`vflt collective import --mirror board`).

The other kinds parse and fail with a clear not-implemented error, so
configurations written now keep working later.

**Bundles.** `vflt collective export` writes the whole board (items, notes,
events, agents) as JSON lines; `vflt collective import --from bundle.jsonl`
loads one into another collective with ids and timestamps intact — between
store kinds, or from another tracker converted to the same shape.

**Ids.** `ids = "ulid"` (default) mints `vf-<ulid>`; `ids = "seq"` mints
`vf-1`, `vf-2`, ... and accepts the bare number anywhere an id is expected.

**Pointer file.** A repository can keep its collective in a subdirectory and
still be found from anywhere inside it with a `.vflt` file at its root:

```toml
path = "ops/fleet"
```

## Collectives and lending

```sh
vflt agent add --name coder-2 --profile coder            # register without running
vflt agent lend coder-2 --to ../other-project/.vflt       # work another board
vflt --collective ../other-project/.vflt agent run --profile coder --name coder-2
```

A collective is local to one machine by default. Lending points an agent at
another collective's board; in this release that board must be on the same
filesystem. Lending to a url needs an http store.

## Development

```sh
cargo build
cargo test            # unit tests + CLI tests, including end-to-end runs with a fake agent
cargo clippy --all-targets
```

The end-to-end tests drive the real `vflt` binary with `fake-agent`, a small
Rust fixture that behaves like an agent (complete, bounce, wait, block, raise,
hang, file work, snooze, do nothing). No shell scripts, so the suite runs
unchanged on Windows.

## License

Apache-2.0. See [LICENSE](LICENSE).
