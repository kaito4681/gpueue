//! The accept loop of the API socket.
//!
//! Each connection is served as HTTP/1 by hyper, with the peer's uid
//! (`SO_PEERCRED`) attached to its requests (see [`PeerCred`]). Unlike
//! `axum::serve`, connections are limited (see [`crate::limits`]): per uid
//! and in total, and a connection that does not send a request's headers
//! within [`HEADER_TIMEOUT`] (including an idle keep-alive connection
//! between requests) is closed. A connection over the limit gets one
//! `too_many_requests` answer and is closed.
//!
//! When the daemon stops, the loop stops accepting and every connection
//! finishes the request it is serving (streams end with the daemon) before
//! [`serve`] returns.

use std::convert::Infallible;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::response::IntoResponse;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use puzuz_protocol::api::ErrorCode;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_service::Service;

use crate::api::PeerCred;
use crate::error::Failure;
use crate::limits::{HEADER_TIMEOUT, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_UID, UidLimiter};

/// Connections being refused at once at most; more are closed right away.
const MAX_REFUSING: usize = 256;

/// How long a connection that is refused may take to send its request.
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Serves `router` on `listener` until `cancel` fires and every open
/// connection has finished.
pub async fn serve(listener: UnixListener, router: Router, cancel: CancellationToken) {
    let limiter = UidLimiter::new(MAX_CONNECTIONS_PER_UID, MAX_CONNECTIONS);
    let refusing = Arc::new(Semaphore::new(MAX_REFUSING));
    let mut connections = JoinSet::new();
    let mut last_warning: Option<Instant> = None;
    let mut warn = |message: String| {
        // At most one warning every 10 s: a flood is logged, not echoed.
        if last_warning.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
            tracing::warn!("{message}");
            last_warning = Some(Instant::now());
        }
    };
    loop {
        while connections.try_join_next().is_some() {}
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Out of file descriptors, for instance: back off rather
                // than spin.
                warn(format!("cannot accept an API connection: {e}"));
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                continue;
            }
        };
        let uid = match stream.peer_cred() {
            Ok(cred) => Some(cred.uid()),
            Err(e) => {
                tracing::warn!("cannot read peer credentials: {e}");
                None
            }
        };
        // Connections without credentials share one budget; their requests
        // are refused anyway.
        let permit = limiter.try_acquire(uid.unwrap_or(u32::MAX));
        if permit.is_none() {
            warn(format!(
                "too many API connections (uid {}); refusing new ones",
                uid.map_or_else(|| "unknown".to_owned(), |uid| uid.to_string())
            ));
        }
        let router = router.clone();
        let cancel = cancel.clone();
        let refusing = refusing.clone();
        connections.spawn(async move {
            match permit {
                Some(_permit) => serve_connection(stream, router, PeerCred { uid }, cancel).await,
                None => {
                    // Beyond that, close them unanswered.
                    if let Ok(_refusing) = refusing.try_acquire() {
                        refuse_connection(stream).await;
                    }
                }
            }
        });
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
}

async fn serve_connection(
    stream: UnixStream,
    router: Router,
    peer: PeerCred,
    cancel: CancellationToken,
) {
    let service = service_fn(move |mut request: hyper::Request<Incoming>| {
        request.extensions_mut().insert(ConnectInfo(peer));
        let mut router = router.clone();
        async move { router.call(request).await }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    let mut connection = pin!(connection);
    let result = tokio::select! {
        result = connection.as_mut() => result,
        _ = cancel.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(e) = result {
        tracing::debug!("API connection ended: {e}");
    }
}

/// Answers the connection's first request with `too_many_requests`, then
/// closes it.
async fn refuse_connection(stream: UnixStream) {
    let service = service_fn(|_request: hyper::Request<Incoming>| async {
        Ok::<_, Infallible>(
            Failure::new(
                ErrorCode::TooManyRequests,
                format!(
                    "too many open connections to the daemon (at most \
                     {MAX_CONNECTIONS_PER_UID} per user); close some and try again"
                ),
            )
            .into_response(),
        )
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(REFUSAL_TIMEOUT)
        .keep_alive(false);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    let _ = tokio::time::timeout(REFUSAL_TIMEOUT * 2, connection).await;
}
