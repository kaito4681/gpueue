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
