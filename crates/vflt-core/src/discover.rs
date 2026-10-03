//! Locating the collective root.
//!
//! Order: explicit path, `VFLT_COLLECTIVE`, nearest ancestor of the cwd that
//! holds `collective.toml` (directly or under `.vflt/`), then `./.vflt`.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const CONFIG_FILE: &str = "collective.toml";
pub const DEFAULT_DIR: &str = ".vflt";
pub const ENV_VAR: &str = "VFLT_COLLECTIVE";

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
        if d.join(CONFIG_FILE).is_file() {
            return Some(d.to_path_buf());
        }
        let nested = d.join(DEFAULT_DIR);
        if nested.join(CONFIG_FILE).is_file() {
            return Some(nested);
        }
        dir = d.parent();
    }
    None
}

/// Accept either the collective root or a directory that contains `.vflt/`.
fn check(p: &Path) -> Option<PathBuf> {
    if p.join(CONFIG_FILE).is_file() {
        return Some(p.to_path_buf());
    }
    let nested = p.join(DEFAULT_DIR);
    if nested.join(CONFIG_FILE).is_file() {
        return Some(nested);
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
}
