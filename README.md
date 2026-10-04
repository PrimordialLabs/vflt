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

```sh
cargo install --path crates/vflt
```

Requires Rust 1.85+ and, for the default git-tracked store, `git` on PATH.
Builds and tests on macOS, Linux and Windows.

## Quickstart

```sh
# 1. a collective: a board plus profiles, as git-tracked files in ./.vflt
vflt collective init --name payments

# 2. items: an immutable spec plus mutable workflow state
vflt item add --title "Survey the target AWS account" --stage survey --spec-file survey.md
vflt item add --title "RDS module for payments DB" --stage code --spec "Write the Terraform module..." --priority 20
vflt board

# 3. an agent working the board with the default headless Claude Code runner
vflt agent run --profile coder

# 4. or take an item yourself
vflt item claim vf-01m3z --me        # unique id prefixes are fine
vflt item start vf-01m3z
vflt item note vf-01m3z --body "started; using the existing VPC module"
vflt item complete vf-01m3z --note "module written, plan is clean"

# 5. watch
vflt item list
vflt events --since 2h
vflt item show vf-01m3z
```

Every command takes `--json`. The collective is found from `--collective`,
`VFLT_COLLECTIVE`, the nearest ancestor holding `collective.toml`, then
`./.vflt`.

## How work flows

Items carry a **stage**. The default pipeline is

```
survey -> plan -> code -> review -> test -> deploy -> done
```

and `vflt item complete` advances an item one stage; a reviewer or tester
uses `complete --bounce` to send it back to `code`. Profiles declare which
stages they handle, so a `coder` agent only ever claims `code` items. Items
can depend on other items (`--dep`), be delegated to an agent, a human or a
whole profile (`--assign coder`), and carry an external ticket reference
(`item link`).

| Verb      | From                 | To                      |
|-----------|----------------------|-------------------------|
| claim     | pending              | claimed (atomic)        |
| start     | claimed or pending   | in_progress             |
| complete  | in_progress          | next stage, pending     |
| block     | in_progress          | blocked                 |
| raise     | in_progress          | needs_human             |
| handoff   | any open             | pending, new assignee   |
| reopen    | blocked, needs_human | pending                 |
| cancel    | any open             | cancelled               |

Claims are atomic (`create_new` on a `.claim` file). A claim whose holder has
stopped heartbeating goes stale after `claim_ttl` and can be released by
anyone; `item release --force` breaks any claim and records that it did.

## Profiles

A profile is a role: `profiles/<name>.toml` (stages, runner, policy, budget,
optional cycle) plus `profiles/<name>.md` (the system prompt). Seven ship by
default and are copied into every new collective:

| profile    | stages  | notes                                            |
|------------|---------|--------------------------------------------------|
| supervisor | (cycle) | runs every 15m; files, reprioritises, escalates   |
| surveyor   | survey  | read-only discovery, records facts with sources   |
| planner    | plan    | turns findings into small items                   |
| coder      | code    | implements inside the item workspace              |
| reviewer   | review  | verifies against the spec; `complete --bounce`    |
| tester     | test    | exercises end to end in disposable resources      |
| deployer   | deploy  | applies exactly the steps in the spec             |

Edit them in place; they are plain files. The policy section is what the
runner is launched with:

```toml
[policy]
permission_mode  = "auto"
allowed_tools    = ["Read", "Edit", "Bash(git *)", "Bash(vflt *)"]
disallowed_tools = ["Bash(rm -rf *)"]

[policy.classifier]       # advisory rules for the runner's permission classifier
environment = ["Workspace is the item's checkout under the collective root."]
allow       = ["Running the project's own build and test commands."]
soft_deny   = ["Pushing to any remote branch other than the item's branch."]
hard_deny   = ["Deleting files outside the workspace."]

[budget]
max_turns  = 60
wall_clock = "45m"
```

## Runners

A runner executes one agent turn as a child process. The agent is told, in
its system prompt, exactly which `vflt item ...` command to finish with. If it
exits without one, the loop marks the item `needs_human` with the transcript
tail as a note.

- **`claude`** (default): headless Claude Code. `claude -p --permission-mode
  <mode> --permission-prompts none --allowedTools ... --max-turns N
  --output-format stream-json`, with the classifier rules passed as the
  `autoMode` settings block. Transcripts land in `work/<item>/runs/`.
- **`shell`**: any command template, split shlex-style and spawned directly
  (never through a shell). Set `runner = "shell"` and `runner_command = "..."`
  in the profile. The prompt arrives on stdin; `VFLT_PROMPT_FILE`,
  `VFLT_COLLECTIVE`, `VFLT_ITEM` and `VFLT_AGENT` are set.
- **custom**: name a command template once in the user config and refer to it
  from any profile with `runner = "codex"`:

```toml
# <config dir>/vflt/config.toml   (~/.config/vflt on Linux, ~/Library/Application Support/vflt on macOS, %APPDATA%\vflt on Windows)
[runners.codex]
command = "codex exec --full-auto"
```

Wall-clock budgets are enforced by killing the child.

## Store

`collective.toml` names the source of truth:

```toml
[store]
kind = "file"   # file | sqlite | turso | dynamodb | mysql | http
root = "."
git  = true     # commit every board mutation
```

Only `file` is implemented in this release: items, agents, profiles and events
are plain files under the collective root, and with `git = true` every state
change is a commit whose message is the event. The other kinds parse and fail
with a clear not-implemented error, so configurations written now keep
working later.

```
.vflt/
├── collective.toml
├── items/<id>/{spec.md, state.toml, notes/, events.jsonl, .claim}
├── agents/<id>.toml          (+ <id>.hb heartbeat, not tracked)
├── profiles/<name>.{toml,md}
├── events.jsonl
└── work/<id>/                (per-item workspaces and runner transcripts, not tracked)
```

## Collectives and lending

```sh
vflt agent add --name coder-2 --profile coder            # register without running
vflt agent lend coder-2 --to ../other-project/.vflt       # work another board
vflt --collective ../other-project/.vflt agent run --profile coder --name coder-2
```

A collective is local to one machine by default. Lending points an agent at
another collective's board; in this release that board must be a file store
on the same filesystem. Lending to a url needs a database or http store.

## Development

```sh
cargo build
cargo test            # unit tests + CLI tests, including an end-to-end run with a fake agent
cargo clippy --all-targets
```

The end-to-end tests drive the real `vflt` binary with `fake-agent`, a small
Rust fixture that behaves like an agent (complete, bounce, block, raise,
hang, do nothing). No shell scripts, so the suite runs unchanged on Windows.

## License

Apache-2.0. See [LICENSE](LICENSE).
