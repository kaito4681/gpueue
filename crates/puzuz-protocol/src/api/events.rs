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

/// One message decoded from a `text/event-stream` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    /// The `event:` field, if present.
    pub event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

impl SseMessage {
    /// Parses `data` as an [`Event`]. Fails for event types this version
    /// does not know, which consumers should skip for forward compatibility.
    pub fn parse_event(&self) -> Result<Event, serde_json::Error> {
        serde_json::from_str(&self.data)
    }
}

/// Incremental decoder of a Server-Sent Events stream.
///
/// Feed it the body chunks as they arrive; it returns every message that is
/// complete so far. Comments (`:` lines, used as keep-alives) and fields
/// other than `event` and `data` are ignored. Pure, so it is usable without
/// the `client` feature.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes of an incomplete line.
    partial: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `chunk` and returns the messages completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        let mut messages = Vec::new();
        self.partial.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(offset) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            let mut line = &self.partial[start..end];
            if let [rest @ .., b'\r'] = line {
                line = rest;
            }
            let line = String::from_utf8_lossy(line).into_owned();
            if let Some(message) = self.handle_line(&line) {
                messages.push(message);
            }
            start = end + 1;
        }
        self.partial.drain(..start);
        messages
    }

    fn handle_line(&mut self, line: &str) -> Option<SseMessage> {
        if line.is_empty() {
            let event = self.event.take();
            return self.data.take().map(|data| SseMessage { event, data });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_messages_split_across_chunks() {
        let event = Event::JobRemoved {
            id: puzuz_core::model::JobId(3),
        };
        let wire = format!(": keep-alive\n\n{}", event.to_sse());
        let (a, b) = wire.as_bytes().split_at(17);
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(a).is_empty());
        let messages = decoder.push(b);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].event.as_deref(), Some("job_removed"));
        assert_eq!(messages[0].parse_event().unwrap(), event);
    }

    #[test]
    fn joins_multiline_data_and_accepts_crlf() {
        let mut decoder = SseDecoder::new();
        let messages = decoder.push(b"data: a\r\ndata:b\r\n\r\nevent: x\ndata\n\n");
        assert_eq!(
            messages,
            vec![
                SseMessage {
                    event: None,
                    data: "a\nb".into()
                },
                SseMessage {
                    event: Some("x".into()),
                    data: String::new()
                },
            ]
        );
    }
}
