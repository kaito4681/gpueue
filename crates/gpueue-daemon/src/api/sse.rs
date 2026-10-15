//! `GET /v1/events`: Server-Sent Events of job and group changes.
//!
//! Job payloads are rendered per subscriber, so users only ever receive the
//! redacted view of other users' jobs.
//!
//! A subscriber that falls more than the channel's capacity behind misses
//! events; it is sent [`api::Event::Resync`](gpueue_protocol::api::Event)
//! instead, so it re-reads the state rather than waiting for a change it
//! will never hear about.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use gpueue_core::model::Caller;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{AppState, Auth};
use crate::error::Failure;
use crate::limits::UidPermit;
use crate::state::DaemonEvent;

struct Subscription {
    events: broadcast::Receiver<DaemonEvent>,
    caller: Caller,
    cancel: CancellationToken,
    done: bool,
    /// Counts against the caller's stream limit while open.
    _permit: UidPermit,
}

pub async fn events(
    State(core): State<AppState>,
    Auth(caller): Auth,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, Failure> {
    let permit = core
        .streams
        .try_acquire(caller.uid.0)
        .ok_or_else(super::log::too_many_streams)?;
    let subscription = Subscription {
        events: core.subscribe(),
        caller,
        cancel: core.cancel.clone(),
        done: false,
        _permit: permit,
    };
    let stream = futures::stream::unfold(subscription, |mut sub| async move {
        if sub.done {
            return None;
        }
        let event = tokio::select! {
            _ = sub.cancel.cancelled() => return None,
            event = sub.events.recv() => event,
        };
        let api_event = match event {
            Ok(event) => {
                sub.done = matches!(event, DaemonEvent::Shutdown);
                event.to_api(&sub.caller)
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::debug!(skipped, "an event subscriber lagged behind; resyncing it");
                gpueue_protocol::api::Event::Resync
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        let data = serde_json::to_string(&api_event).expect("events serialize");
        let sse = SseEvent::default().event(api_event.name()).data(data);
        Some((Ok(sse), sub))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
