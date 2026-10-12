//! `vflt fleet run`: every local agent of the collective in one process, one
//! thread each. Each thread is an ordinary agent loop; the process exits when
//! every agent has stopped (`vflt agent stop --all`). With `--max-parallel`,
//! the agents share that many turn slots, so a fleet larger than its machine
//! takes turns instead of running them all at once.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use vflt_core::AgentKind;

use crate::agent_loop::{self, RunOptions, TurnSlots};
use crate::ctx::Ctx;

pub fn run(
    ctx: &Ctx,
    only: Vec<String>,
    interval: Duration,
    once: bool,
    max_parallel: usize,
) -> Result<()> {
    let agents: Vec<String> = ctx
        .store()
        .list_agents()?
        .into_iter()
        .filter(|a| a.config.kind == AgentKind::Local && a.config.lent_to.is_none())
        .filter(|a| !a.config.profiles.is_empty())
        .map(|a| a.config.name)
        .filter(|n| only.is_empty() || only.contains(n))
        .collect();
    if agents.is_empty() {
        bail!(
            "no local agents to run; register them with `vflt agent add --name <n> --profile <p>`"
        );
    }
    let width = agents.iter().map(|n| n.len()).max().unwrap_or(0);
    let slots = (max_parallel > 0).then(|| TurnSlots::new(max_parallel));
    vflt_core::log_line(&format!(
        "vflt: fleet of {} agents{}: {}",
        agents.len(),
        slots
            .as_ref()
            .map(|s| format!(", at most {} turns at once", s.cap()))
            .unwrap_or_default(),
        agents.join(", ")
    ));
    let stop = crate::shutdown::install();
    let failures: Vec<(String, anyhow::Error)> = std::thread::scope(|s| {
        let handles: Vec<_> = agents
            .iter()
            .enumerate()
            .map(|(n, name)| {
                let stop = Arc::clone(&stop);
                let slots = slots.clone();
                let name = name.clone();
                s.spawn(move || {
                    // Stagger starts so first turns do not all hit the runner at once.
                    std::thread::sleep(Duration::from_millis(250 * n as u64));
                    let opts = RunOptions {
                        name: name.clone(),
                        profile: None,
                        once,
                        interval,
                        runner: None,
                        prefix: format!(" [{name:<width$}]"),
                        stop,
                        slots,
                    };
                    (name, agent_loop::run(ctx, opts))
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| match h.join() {
                Ok((_, Ok(()))) => None,
                Ok((name, Err(e))) => Some((name, e)),
                Err(_) => Some(("?".into(), anyhow::anyhow!("agent thread panicked"))),
            })
            .collect()
    });
    for (name, e) in &failures {
        vflt_core::log_line(&format!("vflt: agent {name} exited with an error: {e:#}"));
    }
    if failures.len() == agents.len() {
        bail!("every agent failed");
    }
    Ok(())
}
