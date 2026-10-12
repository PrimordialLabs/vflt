//! clap definitions. Behaviour lives in `cmd`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "vflt", version, about = "vflt: a standalone fleet orchestrator for agents and humans", long_about = None)]
pub struct Cli {
    /// Collective root (or a directory containing .vflt). Default: nearest ancestor, then ./.vflt
    #[arg(long, global = true, env = "VFLT_COLLECTIVE", value_name = "DIR")]
    pub collective: Option<PathBuf>,

    /// Act as this actor (agent id or username). Default: VFLT_AGENT, else your username
    #[arg(long = "as", global = true, value_name = "ACTOR")]
    pub actor: Option<String>,

    /// Machine-readable output
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create, inspect, export or import a collective
    Collective {
        #[command(subcommand)]
        cmd: CollectiveCmd,
    },
    /// Work items on the board
    Item {
        #[command(subcommand)]
        cmd: ItemCmd,
    },
    /// Agents in the worker pool, and their control plane
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Run every local agent of the collective in one process
    Fleet {
        #[command(subcommand)]
        cmd: FleetCmd,
    },
    /// File a fleet-wide fact once: a new needs_human item, or a note on the
    /// open item that already carries the key
    Flag {
        /// Deduplication key; at most one open item carries it
        #[arg(long)]
        key: String,
        #[arg(long)]
        title: String,
        /// What is wrong and what to do about it
        #[arg(long)]
        body: Option<String>,
        /// Who should see it (default: the filer)
        #[arg(long, value_name = "ACTOR")]
        assign: Option<String>,
        /// decision | action
        #[arg(long, default_value = "action")]
        kind: String,
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        #[arg(long)]
        priority: Option<i32>,
    },
    /// What needs a human: raised items, and things that look stuck
    Inbox {
        /// decision | action | all
        #[arg(long, default_value = "all")]
        kind: String,
    },
    /// One-screen summary of the board
    Board,
    /// Event log
    Events {
        /// Only events for this item
        #[arg(long)]
        item: Option<String>,
        /// RFC3339 timestamp, or a duration ago such as 2h
        #[arg(long)]
        since: Option<String>,
        /// Only events of this kind (e.g. item.completed)
        #[arg(long)]
        kind: Option<String>,
        /// Only events by this actor
        #[arg(long)]
        actor: Option<String>,
        /// Only the most recent N
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Spend recorded by runners
    Cost {
        /// RFC3339 timestamp, or a duration ago such as 7d (default 24h)
        #[arg(long, default_value = "24h")]
        since: String,
        /// agent | day | item
        #[arg(long, default_value = "agent")]
        by: String,
    },
    /// List the profiles in this collective
    Profiles,
}

#[derive(Subcommand, Debug)]
pub enum CollectiveCmd {
    /// Create a collective (default: ./.vflt)
    Init {
        dir: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        /// file | sqlite
        #[arg(long, default_value = "file")]
        store: String,
        /// Do not commit board changes to git (file store)
        #[arg(long)]
        no_git: bool,
        /// pull | delegated | both
        #[arg(long, default_value = "pull")]
        claim_mode: String,
        /// ulid | seq
        #[arg(long, default_value = "ulid")]
        ids: String,
        /// Keep a plain-file mirror of every item in this directory (sqlite store)
        #[arg(long, value_name = "DIR")]
        mirror: Option<String>,
        /// Comma-separated stages, the last one terminal (default survey..done)
        #[arg(long, value_delimiter = ',')]
        pipeline: Vec<String>,
    },
    /// Show the collective configuration and location
    Show,
    /// Write the whole board as JSON lines, or rebuild the mirror
    Export {
        /// Output file, or - for stdout
        #[arg(long, default_value = "-", conflicts_with = "mirror")]
        to: PathBuf,
        /// Rewrite every mirror file instead
        #[arg(long)]
        mirror: bool,
    },
    /// Load a JSON-lines bundle (or a mirror directory), keeping ids and times
    Import {
        /// Bundle file, or - for stdin
        #[arg(long, conflicts_with = "mirror")]
        from: Option<PathBuf>,
        /// A mirror directory to rebuild the board from
        #[arg(long, value_name = "DIR")]
        mirror: Option<PathBuf>,
    },
}

#[derive(Args, Debug, Default)]
pub struct SpecInput {
    /// Spec text inline
    #[arg(long, conflicts_with = "spec_file")]
    pub spec: Option<String>,
    /// Spec from a file, or - for stdin
    #[arg(long, value_name = "FILE")]
    pub spec_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum ItemCmd {
    /// File a new item
    Add {
        #[arg(long)]
        title: String,
        /// Short handle (default: derived from the title)
        #[arg(long)]
        slug: Option<String>,
        /// A plain-language summary for people scanning the board
        #[arg(long)]
        summary: Option<String>,
        /// Stage (default: first stage of the pipeline)
        #[arg(long)]
        stage: Option<String>,
        /// Lower runs first (default 50)
        #[arg(long)]
        priority: Option<i32>,
        /// Delegate to an agent, a human, or a profile name
        #[arg(long, value_name = "ACTOR")]
        assign: Option<String>,
        /// Items that must be done first (repeatable)
        #[arg(long = "dep", value_name = "ID")]
        deps: Vec<String>,
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// The item this one follows up on
        #[arg(long, value_name = "ID")]
        parent: Option<String>,
        /// File as a draft: not claimable until promoted, spec still editable
        #[arg(long)]
        draft: bool,
        #[command(flatten)]
        spec: SpecInput,
    },
    /// List items (live items by default: no drafts, nothing closed)
    List {
        #[arg(long)]
        stage: Option<String>,
        /// Any of these statuses (comma-separated or repeated)
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// None of these statuses
        #[arg(long, value_delimiter = ',')]
        not_status: Vec<String>,
        #[arg(long)]
        assignee: Option<String>,
        /// Items assigned to me
        #[arg(long, conflicts_with = "assignee")]
        mine: bool,
        #[arg(long)]
        owner: Option<String>,
        /// Any of these tags
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
        /// None of these tags
        #[arg(long, value_delimiter = ',')]
        not_tag: Vec<String>,
        #[arg(long, value_name = "ID")]
        parent: Option<String>,
        #[arg(long)]
        key: Option<String>,
        /// Only items not updated for this long (e.g. 24h)
        #[arg(long, value_name = "DURATION")]
        stale: Option<String>,
        /// Include drafts and closed items
        #[arg(long)]
        all: bool,
        /// priority | created | updated
        #[arg(long, default_value = "priority")]
        sort: String,
        /// Reverse the order
        #[arg(long)]
        desc: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Show one item with its spec and notes
    Show {
        id: String,
        /// Include the item's event trail
        #[arg(long)]
        events: bool,
    },
    /// Claim an item (atomic)
    Claim {
        id: String,
        /// Claim as yourself (your username), ignoring VFLT_AGENT
        #[arg(long)]
        me: bool,
    },
    /// Mark a claimed item in progress (claims it first if pending)
    Start { id: String },
    /// Append a note (body from --body, --body-file, or stdin)
    Note {
        id: String,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long, value_name = "FILE")]
        body_file: Option<PathBuf>,
    },
    /// Finish the current stage: advance it, or bounce it back to whoever handed it over
    Complete {
        id: String,
        /// Not acceptable: send it back where it came from
        #[arg(long)]
        bounce: bool,
        /// Send to an explicit stage instead
        #[arg(long, conflicts_with = "bounce")]
        to_stage: Option<String>,
        /// Attach a note
        #[arg(long)]
        note: Option<String>,
    },
    /// Block on something no item will clear (a missing tool, a foreign system)
    Block {
        id: String,
        #[arg(long)]
        on: String,
    },
    /// Raise to a human
    Raise {
        id: String,
        #[arg(long)]
        question: String,
        /// decision (a call to make) | action (something to do)
        #[arg(long, default_value = "decision")]
        kind: String,
    },
    /// Hand to another actor, optionally changing stage
    Handoff {
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Park until other items are done or cancelled
    Wait {
        id: String,
        #[arg(long, value_delimiter = ',', required = true, value_name = "ID")]
        on: Vec<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Reopen a blocked or needs_human item
    Reopen {
        id: String,
        /// Reassign while reopening
        #[arg(long)]
        to: Option<String>,
    },
    /// Answer an item: note it, and send it on to whoever acts next
    Respond {
        id: String,
        #[arg(long)]
        note: String,
        /// Only record the note
        #[arg(long)]
        note_only: bool,
    },
    /// Release a draft onto the board
    Promote { id: String },
    /// Take a pending item back to draft
    Demote { id: String },
    /// Cancel an item
    Cancel {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Release your claim, or break a stale one (--force breaks any claim)
    Release {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// Link to an external ticket
    Link {
        id: String,
        #[arg(long)]
        system: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        url: Option<String>,
    },
    /// Set or clear the assignee
    Assign {
        id: String,
        /// Actor to assign to; omit with --none to return to the pool
        actor: Option<String>,
        #[arg(long, conflicts_with = "actor")]
        none: bool,
    },
    /// Change an item's metadata (and a draft's spec)
    Set {
        id: String,
        #[arg(long)]
        title: Option<String>,
        /// "" clears it
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long, value_name = "ID", conflicts_with = "no_parent")]
        parent: Option<String>,
        #[arg(long)]
        no_parent: bool,
        #[command(flatten)]
        spec: SpecInput,
    },
    /// Add or remove tags
    Tag {
        id: String,
        #[arg(long, value_delimiter = ',')]
        add: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        remove: Vec<String>,
    },
    /// Add or remove dependencies
    Dep {
        id: String,
        #[arg(long, value_delimiter = ',', value_name = "ID")]
        add: Vec<String>,
        #[arg(long, value_delimiter = ',', value_name = "ID")]
        remove: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Register an agent without running it
    Add {
        #[arg(long)]
        name: String,
        #[arg(long = "profile", value_name = "PROFILE")]
        profiles: Vec<String>,
        /// Mark as a remote agent reachable at this url
        #[arg(long, conflicts_with = "human")]
        remote: Option<String>,
        /// A person who works items by hand
        #[arg(long)]
        human: bool,
    },
    /// Register and work the board with a profile
    Run {
        #[arg(long)]
        profile: Option<String>,
        /// Work one item (or one cycle) and exit
        #[arg(long)]
        once: bool,
        /// Poll interval when idle (e.g. 15s)
        #[arg(long, default_value = "15s")]
        interval: String,
        /// Agent name (default: <profile>@<host>)
        #[arg(long)]
        name: Option<String>,
        /// Override the profile's runner
        #[arg(long)]
        runner: Option<String>,
    },
    /// Point a local agent at another collective's board
    Lend {
        agent: String,
        /// Collective root path (urls need a database or http store)
        #[arg(long, value_name = "COLLECTIVE")]
        to: String,
    },
    /// List registered agents with their control plane, runtime and queue
    List,
    /// Deregister an agent
    Remove { name: String },
    /// Take no new work until resumed
    Pause {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Go back to work
    Resume {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Finish the current turn, then exit
    Stop {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Run a cycle turn at the next opportunity
    Wake { name: String },
    /// on: work only what others route here (no cycle turns, no self-filed items)
    Hold { name: String, state: String },
    /// Caught up: no cycle turns until work is routed here again
    Snooze {
        /// Default: the acting agent (VFLT_AGENT or --as)
        name: Option<String>,
    },
    /// Override a profile setting for one agent ("none" clears)
    Set {
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        cycle: Option<String>,
        #[arg(long)]
        wall_clock: Option<String>,
        #[arg(long)]
        daily_usd: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FleetCmd {
    /// Work the board with every local agent, one thread each
    Run {
        /// Only these agents (comma-separated)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Poll interval when idle (e.g. 15s)
        #[arg(long, default_value = "15s")]
        interval: String,
        /// Each agent works one item (or one cycle) and exits
        #[arg(long)]
        once: bool,
        /// Run at most this many turns at once across the fleet (0: no cap);
        /// agents with work wait for a free slot before claiming it
        #[arg(long, default_value_t = 0)]
        max_parallel: usize,
    },
}
