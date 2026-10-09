//! Server-Sent Events of `GET /v1/events`.

use puzuz_core::model::{Group, JobId};
use serde::{Deserialize, Serialize};

use super::JobView;

/// One SSE message. On the wire the SSE `event:` field is
/// [`Event::name`] and `data:` is this value as single-line JSON (which also
/// carries the name in `type`, so consumers may ignore the `event:` field).
///
/// Job views are built per subscriber with [`JobView::for_caller`], so
/// non-owners only ever see redacted jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobAdded {
        job: JobView,
    },
    /// Any change of a job, most notably of its state (used by `wait`).
    JobUpdated {
        job: JobView,
    },
    JobRemoved {
        id: JobId,
    },
    GroupUpdated {
        group: Group,
    },
    GroupRemoved {
        name: String,
    },
    /// The daemon is shutting down; the stream ends afterwards.
    DaemonShutdown,
}

impl Event {
    /// The SSE `event:` field.
    pub fn name(&self) -> &'static str {
        match self {
            Event::JobAdded { .. } => "job_added",
            Event::JobUpdated { .. } => "job_updated",
            Event::JobRemoved { .. } => "job_removed",
            Event::GroupUpdated { .. } => "group_updated",
            Event::GroupRemoved { .. } => "group_removed",
            Event::DaemonShutdown => "daemon_shutdown",
        }
    }

    /// Renders one complete SSE message (`event:`, `data:` and the blank
    /// line terminator).
    pub fn to_sse(&self) -> String {
        // serde_json escapes newlines inside strings, so `data` is one line.
        let data = serde_json::to_string(self).expect("events serialize");
        format!("event: {}\ndata: {}\n\n", self.name(), data)
    }
}
