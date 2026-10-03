use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("item not found: {0}")]
    ItemNotFound(String),
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error("item {item} is already claimed by {holder}")]
    AlreadyClaimed { item: String, holder: String },
    #[error("item {item} is not claimed")]
    NotClaimed { item: String },
    #[error("item {item} is claimed by {holder}, not {actor}; use --force to override")]
    NotClaimHolder {
        item: String,
        holder: String,
        actor: String,
    },
    #[error("invalid transition for item {item}: {verb} from status {from}")]
    InvalidTransition {
        item: String,
        verb: &'static str,
        from: String,
    },
    #[error("store kind `{0}` is not implemented yet; only `file` is available in this release")]
    StoreNotImplemented(String),
    #[error("no collective found: looked for collective.toml in {0} and its ancestors, then ./.vflt; pass --collective or set VFLT_COLLECTIVE")]
    CollectiveNotFound(PathBuf),
    #[error("collective already exists at {0}")]
    CollectiveExists(PathBuf),
    #[error("invalid {what}: {detail}")]
    Invalid { what: &'static str, detail: String },
    #[error("git: {0}")]
    Git(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn invalid(what: &'static str, detail: impl Into<String>) -> Self {
        Error::Invalid {
            what,
            detail: detail.into(),
        }
    }
}
