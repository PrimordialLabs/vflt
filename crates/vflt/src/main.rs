//! `vflt`: the fleet orchestrator CLI. See `docs/DESIGN.md`.

mod agent_loop;
mod cli;
mod cmd;
mod ctx;
mod fleet;
mod identity;
mod prompt;
mod shutdown;
mod util;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    if let Err(e) = cmd::dispatch(args) {
        vflt_core::log_line(&format!("vflt: {e:#}"));
        std::process::exit(1);
    }
}
