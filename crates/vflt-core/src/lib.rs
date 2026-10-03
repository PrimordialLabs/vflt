//! vflt-core: types, the `Store` trait, and the git-tracked file store for the
//! vflt fleet orchestrator. See `docs/DESIGN.md` at the repo root.

pub mod collective;
pub mod defaults;
pub mod discover;
pub mod duration;
pub mod error;
pub mod file_store;
pub mod git;
pub mod profile;
pub mod selection;
pub mod store;
pub mod types;

pub use collective::{ClaimMode, Collective, StoreConfig, StoreKind};
pub use error::{Error, Result};
pub use file_store::FileStore;
pub use profile::{Budget, Classifier, PermissionMode, Policy, Profile};
pub use selection::select_next;
pub use store::{open_store, Store};
pub use types::*;
