//! Conversion of internal errors into API errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gpueue_core::lifecycle::{self, Action};
use gpueue_core::model::JobState;
use gpueue_core::storage::StorageError;
use gpueue_protocol::api::{ApiError, ErrorCode};

use crate::helper::HelperError;

/// An [`ApiError`] that can be returned from handlers and built with `?`
/// from storage, domain and helper errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub ApiError);

pub type ApiResult<T> = Result<T, Failure>;

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self(ApiError::new(code, message))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    pub fn code(&self) -> ErrorCode {
        self.0.code
    }

    /// An invalid-transition error with the user hint for `action`.
    pub fn invalid_transition(err: gpueue_core::Error, action: Action, state: &JobState) -> Self {
        let mut failure = Failure::from(err);
        if let Some(hint) = lifecycle::hint(action, state) {
            failure.0.message = format!("{} ({hint})", failure.0.message);
        }
        failure
    }
}

impl From<ApiError> for Failure {
    fn from(err: ApiError) -> Self {
        Self(err)
    }
}

impl From<gpueue_core::Error> for Failure {
    fn from(err: gpueue_core::Error) -> Self {
        Self(ApiError::from(err))
    }
}

impl From<StorageError> for Failure {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::Domain(e) => e.into(),
            StorageError::GroupExists(_) => Failure::new(ErrorCode::AlreadyExists, err.to_string()),
            StorageError::GroupNotEmpty(_) => {
                Failure::new(ErrorCode::InvalidState, err.to_string())
            }
            StorageError::DefaultGroupRemoval => Failure::bad_request(err.to_string()),
            StorageError::CheckpointNotFound(_) => {
                Failure::new(ErrorCode::InvalidState, err.to_string())
            }
            other => {
                tracing::error!("storage error: {other}");
                Failure::internal(other.to_string())
            }
        }
    }
}

impl From<HelperError> for Failure {
    fn from(err: HelperError) -> Self {
        let code = match &err {
            HelperError::NotConnected | HelperError::ConnectionLost | HelperError::Timeout => {
                ErrorCode::HelperUnavailable
            }
            HelperError::Rejected { .. } | HelperError::Protocol(_) => ErrorCode::Internal,
        };
        Failure::new(code, format!("gpueue helper: {err}"))
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0)).into_response()
    }
}
