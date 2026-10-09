use crate::model::{GpuIndex, JobId};

/// Errors produced by the core domain logic.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("job {0} does not exist")]
    JobNotFound(JobId),

    #[error("group {0:?} does not exist")]
    GroupNotFound(String),

    #[error("GPU {0} does not exist")]
    GpuNotFound(GpuIndex),

    #[error("permission denied")]
    PermissionDenied,

    #[error("job {id} cannot be {action} while it is {state}")]
    InvalidTransition {
        id: JobId,
        action: &'static str,
        state: &'static str,
    },
}

/// Errors produced by the `storage` module.
///
/// Kept separate from [`Error`] because the underlying SQLite and I/O errors
/// are neither `Clone` nor `Eq`. Domain errors (such as a missing job) are
/// wrapped in [`StorageError::Domain`] so callers can still match on them.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Domain(#[from] Error),

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("cannot prepare database file {}: {source}", path.display())]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "database schema version {found} is newer than the newest version this puzuz \
         understands ({supported}); upgrade puzuz or restore a matching database"
    )]
    UnsupportedSchemaVersion { found: i64, supported: i64 },

    #[error("corrupt database contents: {0}")]
    Corrupt(String),

    #[error("group {0:?} already exists")]
    GroupExists(String),

    #[error("group {0:?} still contains jobs")]
    GroupNotEmpty(String),

    #[error("job {0} has no checkpoint record")]
    CheckpointNotFound(JobId),

    #[error("the {:?} group cannot be removed", crate::model::DEFAULT_GROUP)]
    DefaultGroupRemoval,
}
