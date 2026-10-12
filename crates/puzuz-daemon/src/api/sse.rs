//! `GET /v1/events`: Server-Sent Events of job and group changes.
//!
//! Job payloads are rendered per subscriber, so users only ever receive the
//! redacted view of other users' jobs.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use puzuz_core::model::Caller;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{AppState, Auth};
use crate::state::DaemonEvent;

struct Subscription {
    events: broadcast::Receiver<DaemonEvent>,
    caller: Caller,
    cancel: CancellationToken,
    done: bool,
}

pub async fn events(
    State(core): State<AppState>,
    Auth(caller): Auth,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let subscription = Subscription {
        events: core.subscribe(),
        caller,
        cancel: core.cancel.clone(),
        done: false,
    };
    let stream = futures::stream::unfold(subscription, |mut sub| async move {
        if sub.done {
            return None;
        }
        loop {
            let event = tokio::select! {
                _ = sub.cancel.cancelled() => return None,
                event = sub.events.recv() => event,
            };
            match event {
                Ok(event) => {
                    sub.done = matches!(event, DaemonEvent::Shutdown);
                    let api_event = event.to_api(&sub.caller);
                    let data = serde_json::to_string(&api_event).expect("events serialize");
                    let sse = SseEvent::default().event(api_event.name()).data(data);
                    return Some((Ok(sse), sub));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "an event subscriber lagged behind");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
