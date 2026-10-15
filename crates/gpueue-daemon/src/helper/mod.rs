//! The daemon's side of the root helper connection.
//!
//! [`Helper`] abstracts the helper so the daemon can be tested without root:
//! [`HelperClient`] talks to the real `gpueue-helper` over its Unix socket,
//! [`FakeHelper`] simulates processes in memory.
//!
//! Besides request/response calls, a helper delivers [`HelperNotice`]s on
//! the channel returned by [`Helper::connect`]:
//! - [`HelperNotice::Event`]: job events, possibly replayed after a
//!   reconnect (the daemon acknowledges them with [`Helper::ack_events`]
//!   once recorded, and must handle duplicates: it remembers the sequence
//!   number of the last event it applied and skips replays up to it);
//! - [`HelperNotice::UndecodableEvent`]: an event that could not be
//!   decoded, only to be acknowledged;
//! - [`HelperNotice::Connected`]: the connection is (re)established and every
//!   replayed event sent before it has been delivered; carries the helper's
//!   live jobs so the daemon can reconcile its database;
//! - [`HelperNotice::Disconnected`].

mod client;
mod fake;

use std::path::PathBuf;

use futures::future::BoxFuture;
use gpueue_core::model::JobId;
use gpueue_protocol::helper::{
    CheckpointOptions, EventEnvelope, EventSeq, HelperErrorCode, RestoreRequest, RunningJob,
    SpawnRequest,
};
use tokio::sync::mpsc;

pub use client::HelperClient;
pub use fake::{FakeHelper, FakeJob, FakeOutcome};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelperError {
    /// No connection to the helper; the request was not sent.
    #[error("the gpueue helper is not connected")]
    NotConnected,
    /// The connection broke after the request was sent; whether it took
    /// effect is unknown (reconciliation will tell).
    #[error("the connection to the gpueue helper was lost")]
    ConnectionLost,
    #[error("the gpueue helper did not answer in time")]
    Timeout,
    /// The helper refused or failed the request.
    #[error("{message}")]
    Rejected {
        code: HelperErrorCode,
        message: String,
    },
    /// The helper answered with an unexpected message.
    #[error("unexpected answer from the gpueue helper: {0}")]
    Protocol(String),
}

/// See the [module documentation](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperNotice {
    Connected {
        version: String,
        running: Vec<RunningJob>,
    },
    Disconnected,
    Event(EventEnvelope),
    /// An event this daemon cannot decode (e.g. one a newer helper added).
    /// The daemon logs and acknowledges it, so that it is not replayed
    /// forever.
    UndecodableEvent {
        seq: EventSeq,
        error: String,
    },
}

/// Connection state, for `GET /v1/version` and to decide whether jobs can
/// be launched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperState {
    pub connected: bool,
    pub version: Option<String>,
    /// See [`gpueue_protocol::helper::Hello::missing_programs`].
    pub missing_programs: Vec<String>,
    /// See [`gpueue_protocol::helper::Hello::programs`].
    pub programs: Vec<gpueue_protocol::helper::HelperProgram>,
}

pub type HelperResult<T> = Result<T, HelperError>;

/// Operations on the root helper. All methods are cheap to call
/// concurrently.
pub trait Helper: Send + Sync + 'static {
    /// Starts (or attaches to) the connection and returns the notice
    /// channel. Called once per daemon instance; a later call replaces the
    /// previous channel.
    fn connect(&self) -> mpsc::UnboundedReceiver<HelperNotice>;

    /// Stops the connection (daemon exit). Running jobs are not affected.
    fn close(&self);

    fn state(&self) -> HelperState;

    /// How many event notices ([`HelperNotice::Event`] and
    /// [`HelperNotice::UndecodableEvent`]) have been put on the notice
    /// channel so far. Read right after a request returns, it counts every
    /// event the helper sent before its answer, so the daemon can make sure
    /// it has applied them before acting on the answer (see
    /// `Core::spawn_lost_check`).
    fn events_delivered(&self) -> u64;

    /// Starts a job; returns its PID.
    fn spawn(&self, request: SpawnRequest) -> BoxFuture<'_, HelperResult<u32>>;

    fn signal(&self, job: JobId, signal: i32) -> BoxFuture<'_, HelperResult<()>>;

    /// Starts a checkpoint; the outcome arrives as an event.
    fn checkpoint(
        &self,
        job: JobId,
        image_dir: PathBuf,
        options: CheckpointOptions,
    ) -> BoxFuture<'_, HelperResult<()>>;

    /// Starts a restore; the outcome arrives as an event.
    fn restore(&self, request: RestoreRequest) -> BoxFuture<'_, HelperResult<()>>;

    fn delete_checkpoint(&self, job: JobId, image_dir: PathBuf) -> BoxFuture<'_, HelperResult<()>>;

    fn list_running(&self) -> BoxFuture<'_, HelperResult<Vec<RunningJob>>>;

    fn ack_events(&self, up_to: EventSeq) -> BoxFuture<'_, HelperResult<()>>;
}
