//! Interrupts. The first Ctrl-C (or SIGTERM / SIGHUP) asks every agent loop
//! in this process to finish its current turn and exit, leaving each agent's
//! desired state untouched; a second one aborts the running turns. Items an
//! aborted turn held are released the next time their agent starts.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Install the handler once and return the flag agent loops watch.
pub fn install() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&stop);
    let result = ctrlc::set_handler(move || {
        let n = count.fetch_add(1, Ordering::SeqCst);
        flag.store(true, Ordering::SeqCst);
        if n == 0 {
            vflt_core::log_line(
                "vflt: finishing the current turn(s), then exiting; interrupt again to abort them",
            );
        } else {
            vflt_core::log_line("vflt: aborting running turns");
            vflt_runner::process::abort_children();
            std::process::exit(130);
        }
    });
    if let Err(e) = result {
        vflt_core::log_line(&format!(
            "vflt: warning: interrupts will not be graceful: {e}"
        ));
    }
    stop
}
