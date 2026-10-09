//! Warnings the daemon hands to a user on their next command.
//!
//! The spec asks that the owner of a job whose checkpoint failed is told
//! about it the next time they run a command. The daemon attaches pending
//! warnings for the caller to any successful response in the
//! [`WARNINGS_HEADER`] header (only for requests that carry the
//! [`VERSION_HEADER`](super::VERSION_HEADER), i.e. from the puzuz CLI) and
//! then considers them delivered. The CLI prints them on stderr.
//!
//! The header value is a comma-separated list of `kind=argument` items, e.g.
//! `checkpoint-failed=3, checkpoint-failed=5`. Clients skip kinds they do
//! not know, so new kinds can be added without a new API version. Details
//! (such as the failure reason) are not in the header; they stay in the job
//! view.

use std::fmt;

use puzuz_core::model::JobId;

/// Response header carrying pending [`Warning`]s.
pub const WARNINGS_HEADER: &str = "x-puzuz-warnings";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Warning {
    /// The last checkpoint of this job (owned by the caller) failed.
    CheckpointFailed { job: JobId },
}

const CHECKPOINT_FAILED: &str = "checkpoint-failed";

impl Warning {
    /// Renders warnings as a [`WARNINGS_HEADER`] value.
    pub fn encode_header(warnings: &[Warning]) -> String {
        warnings
            .iter()
            .map(|warning| match warning {
                Warning::CheckpointFailed { job } => format!("{CHECKPOINT_FAILED}={job}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parses a [`WARNINGS_HEADER`] value, skipping unknown or malformed
    /// items.
    pub fn parse_header(value: &str) -> Vec<Warning> {
        value
            .split(',')
            .filter_map(|item| {
                let (kind, argument) = item.trim().split_once('=')?;
                match kind {
                    CHECKPOINT_FAILED => Some(Warning::CheckpointFailed {
                        job: JobId(argument.parse().ok()?),
                    }),
                    _ => None,
                }
            })
            .collect()
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::CheckpointFailed { job } => write!(
                f,
                "the checkpoint of job {job} failed; it was not saved (see `puzuz status`)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_skips_unknown_items() {
        let warnings = [
            Warning::CheckpointFailed { job: JobId(3) },
            Warning::CheckpointFailed { job: JobId(5) },
        ];
        let header = Warning::encode_header(&warnings);
        assert_eq!(header, "checkpoint-failed=3, checkpoint-failed=5");
        assert_eq!(Warning::parse_header(&header), warnings);
        assert_eq!(
            Warning::parse_header("future-kind=x,checkpoint-failed=7,checkpoint-failed=no, junk"),
            [Warning::CheckpointFailed { job: JobId(7) }]
        );
        assert!(Warning::parse_header("").is_empty());
    }
}
