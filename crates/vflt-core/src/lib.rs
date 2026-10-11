//! vflt-core: types, the item state machine, the `Store` trait, and the two
//! stores that implement it (git-tracked files and sqlite) for the vflt fleet
//! orchestrator. See `docs/DESIGN.md` at the repo root.

pub mod collective;
pub mod defaults;
pub mod discover;
pub mod duration;
pub mod error;
pub mod file_store;
pub mod git;
pub mod machine;
pub mod mirror;
pub mod profile;
pub mod selection;
pub mod sqlite_store;
pub mod store;
pub mod types;

pub use collective::{ClaimMode, Collective, IdStyle, StoreConfig, StoreKind};
pub use error::{Error, Result};
pub use file_store::FileStore;
pub use profile::{Budget, Classifier, PermissionMode, Policy, Profile};
pub use selection::{select_batch, select_next, Picking};
pub use sqlite_store::SqliteStore;
pub use store::{init_store, open_store, Store};
pub use types::*;

/// Write one line to stderr, ignoring failure. A long-running orchestrator
/// must never die because its log pipe closed (`eprintln!` panics then).
pub fn log_line(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}
