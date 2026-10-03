//! clap definitions. Behaviour lives in `cmd`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "vflt", version, about = "vflt: a standalone fleet orchestrator for agents and humans", long_about = None)]
pub struct Cli {
    /// Collective root (or a directory containing .vflt/). Default: nearest ancestor, then ./.vflt
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
    /// Create or inspect a collective
    Collective {
        #[command(subcommand)]
        cmd: CollectiveCmd,
    },
    /// Work items on the board
    Item {
        #[command(subcommand)]
        cmd: ItemCmd,
    },
    /// Agents in the worker pool
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
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
        /// Do not commit board changes to git
        #[arg(long)]
        no_git: bool,
        /// pull | delegated | both
        #[arg(long, default_value = "pull")]
        claim_mode: String,
    },
    /// Show the collective configuration and location
    Show,
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
        #[command(flatten)]
        spec: SpecInput,
    },
    /// List items (open items by default)
    List {
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        /// Items assigned to me
        #[arg(long, conflicts_with = "assignee")]
        mine: bool,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        /// Include done and cancelled items
        #[arg(long)]
        all: bool,
    },
    /// Show one item with its notes
    Show { id: String },
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
    /// Complete the current stage; advances to the next stage or done
    Complete {
        id: String,
        /// Send back to the bounce stage (reviewer / tester)
        #[arg(long)]
        bounce: bool,
        /// Send to an explicit stage instead
        #[arg(long, conflicts_with = "bounce")]
        to_stage: Option<String>,
        /// Attach a note
        #[arg(long)]
        note: Option<String>,
    },
    /// Block on a hard dependency
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
    },
    /// Hand to another actor, optionally changing stage
    Handoff {
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        stage: Option<String>,
    },
    /// Reopen a blocked or needs_human item
    Reopen { id: String },
    /// Cancel an item
    Cancel { id: String },
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
}

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Register an agent without running it
    Add {
        #[arg(long)]
        name: String,
        #[arg(long = "profile", value_name = "PROFILE", required = true)]
        profiles: Vec<String>,
        /// Mark as a remote agent reachable at this url
        #[arg(long)]
        remote: Option<String>,
    },
    /// Register and work the board with a profile
    Run {
        #[arg(long)]
        profile: String,
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
    /// List registered agents
    List,
    /// Deregister an agent
    Remove { name: String },
}
