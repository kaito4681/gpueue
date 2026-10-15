//! Request extractors: the caller's identity and JSON bodies with
//! [`ApiError`](gpueue_protocol::api::ApiError) rejections.

use axum::body::Bytes;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use gpueue_core::model::{Caller, Uid};
use gpueue_protocol::api::ErrorCode;
use serde::de::DeserializeOwned;

use super::AppState;
use crate::error::{ApiResult, Failure};
use crate::limits::ConnectionPermit;

/// Testing only: overrides the peer uid when the daemon was started with
/// `trust_uid_header` (never configurable from the config file).
pub const TEST_UID_HEADER: &str = "x-gpueue-test-uid";

/// Per-connection info: the peer's uid from `SO_PEERCRED`, attached to
/// every request as `ConnectInfo<PeerCred>` (see `crate::server`).
#[derive(Debug, Clone, Copy)]
pub struct PeerCred {
    pub uid: Option<u32>,
}

/// The connection's permit (see [`ConnectionPermit`]), if the request came
/// through `crate::server`.
pub struct Permit(pub Option<ConnectionPermit>);

impl<S: Send + Sync> FromRequestParts<S> for Permit {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Permit(parts.extensions.get::<ConnectionPermit>().cloned()))
    }
}

/// The authenticated caller.
#[derive(Debug, Clone, Copy)]
pub struct Auth(pub Caller);

impl FromRequestParts<AppState> for Auth {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Failure> {
        let uid = peer_uid(&parts.headers, &parts.extensions, state.trust_uid_header)?;
        let users = state.users.clone();
        let is_admin = tokio::task::spawn_blocking(move || users.is_admin(Uid(uid)))
            .await
            .unwrap_or(false);
        Ok(Auth(Caller {
            uid: Uid(uid),
            is_admin,
        }))
    }
}

/// The caller's uid: `SO_PEERCRED`, or the test header when trusted.
pub fn peer_uid(
    headers: &HeaderMap,
    extensions: &Extensions,
    trust_uid_header: bool,
) -> Result<u32, Failure> {
    let peer = extensions
        .get::<ConnectInfo<PeerCred>>()
        .and_then(|info| info.0.uid);
    let mut uid = peer.ok_or_else(|| {
        Failure::new(
            ErrorCode::PermissionDenied,
            "cannot determine who is connecting",
        )
    })?;
    if trust_uid_header && let Some(value) = headers.get(TEST_UID_HEADER) {
        uid = value
            .to_str()
            .ok()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Failure::bad_request("invalid test uid header"))?;
    }
    Ok(uid)
}

/// Parses a body that may be empty (then `T::default()`).
pub fn parse_optional<T: DeserializeOwned + Default>(body: &[u8]) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| Failure::bad_request(format!("invalid body: {e}")))
}

/// A required JSON body.
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = Failure;

    async fn from_request(request: Request, state: &S) -> Result<Self, Failure> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|e| Failure::bad_request(e.body_text()))?;
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(|e| Failure::bad_request(format!("invalid body: {e}")))
    }
}

/// A JSON body that may be omitted.
pub struct OptionalJson<T>(pub T);

impl<T: DeserializeOwned + Default, S: Send + Sync> FromRequest<S> for OptionalJson<T> {
    type Rejection = Failure;

    async fn from_request(request: Request, state: &S) -> Result<Self, Failure> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|e| Failure::bad_request(e.body_text()))?;
        parse_optional(&bytes).map(OptionalJson)
    }
}
