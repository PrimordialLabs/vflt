//! Locating the collective root.
//!
//! Order: explicit path, `VFLT_COLLECTIVE`, then the nearest ancestor of the
//! cwd that holds `collective.toml` directly, holds it under `.vflt/`, or has
//! a `.vflt` *file* pointing elsewhere, then `./.vflt`.
//!
//! The pointer file lets a repository keep its collective in a subdirectory
//! (or a submodule) and still be found from anywhere inside the repository:
//!
//! ```toml
//! # .vflt
//! path = "ops/fleet"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

pub const CONFIG_FILE: &str = "collective.toml";
pub const DEFAULT_DIR: &str = ".vflt";
pub const ENV_VAR: &str = "VFLT_COLLECTIVE";

#[derive(Deserialize)]
struct Pointer {
    path: String,
}

pub fn find(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return check(p).ok_or_else(|| Error::CollectiveNotFound(p.to_path_buf()));
    }
    if let Some(p) = std::env::var_os(ENV_VAR) {
        let p = PathBuf::from(p);
        return check(&p).ok_or(Error::CollectiveNotFound(p));
    }
    find_from(cwd).ok_or_else(|| Error::CollectiveNotFound(cwd.to_path_buf()))
}

/// Search upward from `start` without consulting the environment.
pub fn find_from(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if let Some(found) = check(d) {
            return Some(found);
        }
        dir = d.parent();
    }
    None
}

/// Accept the collective root, a directory that contains `.vflt/`, or a
/// directory with a `.vflt` pointer file.
fn check(p: &Path) -> Option<PathBuf> {
    if p.join(CONFIG_FILE).is_file() {
        return Some(p.to_path_buf());
    }
    let nested = p.join(DEFAULT_DIR);
    if nested.join(CONFIG_FILE).is_file() {
        return Some(nested);
    }
    if nested.is_file() {
        let text = std::fs::read_to_string(&nested).ok()?;
        let ptr: Pointer = toml::from_str(&text).ok()?;
        let target = p.join(ptr.path.trim());
        if target.join(CONFIG_FILE).is_file() {
            return Some(target);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_and_ancestor_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let coll = root.join(".vflt");
        std::fs::create_dir_all(coll.join("deep/er")).unwrap();
        std::fs::write(coll.join(CONFIG_FILE), "").unwrap();
        std::fs::create_dir_all(root.join("src/x")).unwrap();

        assert_eq!(find_from(&root.join("src/x")).unwrap(), coll);
        assert_eq!(find_from(&coll.join("deep/er")).unwrap(), coll);
        assert_eq!(check(&root).unwrap(), coll);
        assert_eq!(check(&coll).unwrap(), coll);
        assert!(find_from(tmp.path()).is_none());
    }

    #[test]
    fn follows_a_pointer_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let target = repo.join("ops/fleet");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(target.join(CONFIG_FILE), "").unwrap();
        std::fs::write(repo.join(DEFAULT_DIR), "path = \"ops/fleet\"\n").unwrap();
        assert_eq!(find_from(&repo.join("src/deep")).unwrap(), target);
        // a broken pointer is not a collective
        std::fs::write(repo.join(DEFAULT_DIR), "path = \"nowhere\"\n").unwrap();
        assert!(find_from(&repo.join("src/deep")).is_none());
    }
}
