use std::fmt;

use serde::{Deserialize, Serialize};

/// Machine-readable error category. Stable on the wire (snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request: bad JSON, invalid field values, unknown GPU index in
    /// a job spec, and so on.
    BadRequest,
    /// The caller is neither the owner of the job nor an administrator, or the
    /// operation is admin-only.
    PermissionDenied,
    JobNotFound,
    GroupNotFound,
    GpuNotFound,
    /// The resource already exists (e.g. creating an existing group).
    AlreadyExists,
    /// The job is in a state that does not allow the operation
    /// (e.g. `kill` on a checkpointed job).
    InvalidState,
    /// The client and daemon versions are incompatible.
    VersionMismatch,
    /// The operation is deliberately not supported (e.g. `send`).
    Unsupported,
    /// The root helper is not connected, so jobs cannot be spawned,
    /// signalled or checkpointed right now.
    HelperUnavailable,
    /// The daemon is shutting down and accepts no new work.
    ShuttingDown,
    Internal,
}

impl ErrorCode {
    /// The HTTP status code the daemon answers with.
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::BadRequest => 400,
            ErrorCode::PermissionDenied => 403,
            ErrorCode::JobNotFound | ErrorCode::GroupNotFound | ErrorCode::GpuNotFound => 404,
            ErrorCode::AlreadyExists | ErrorCode::InvalidState => 409,
            // 400 rather than 426: there is no protocol to upgrade to.
            ErrorCode::VersionMismatch => 400,
            ErrorCode::Unsupported => 501,
            ErrorCode::HelperUnavailable | ErrorCode::ShuttingDown => 503,
            ErrorCode::Internal => 500,
        }
    }

    /// The snake_case wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::JobNotFound => "job_not_found",
            ErrorCode::GroupNotFound => "group_not_found",
            ErrorCode::GpuNotFound => "gpu_not_found",
            ErrorCode::AlreadyExists => "already_exists",
            ErrorCode::InvalidState => "invalid_state",
            ErrorCode::VersionMismatch => "version_mismatch",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::HelperUnavailable => "helper_unavailable",
            ErrorCode::ShuttingDown => "shutting_down",
            ErrorCode::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// JSON body of every error response, and of failed items in bulk results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    /// Human-readable explanation, shown to the user as is.
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<puzuz_core::Error> for ApiError {
    fn from(err: puzuz_core::Error) -> Self {
        use puzuz_core::Error as E;
        let code = match &err {
            E::JobNotFound(_) => ErrorCode::JobNotFound,
            E::GroupNotFound(_) => ErrorCode::GroupNotFound,
            E::GpuNotFound(_) => ErrorCode::GpuNotFound,
            E::PermissionDenied => ErrorCode::PermissionDenied,
            E::InvalidTransition { .. } => ErrorCode::InvalidState,
            // puzuz-core may grow variants (e.g. storage errors) before this
            // mapping is updated.
            #[allow(unreachable_patterns)]
            _ => ErrorCode::Internal,
        };
        ApiError::new(code, err.to_string())
    }
}
