//! Warnings the daemon hands to a user on their next command.
//!
//! The spec asks that the owner of a job whose checkpoint failed is told
//! about it the next time they run a command; a failed restore is reported
//! the same way. The daemon attaches pending warnings for the caller to any
//! successful response in the [`WARNINGS_HEADER`] header (only for requests
//! that carry the [`VERSION_HEADER`](super::VERSION_HEADER), i.e. from the
//! puzuz CLI) and then considers them delivered. The CLI prints them on
//! stderr.
//!
//! Checkpoint images exceeding `checkpoint_size_limit_bytes` are reported the
//! same way, since they are never deleted automatically and a human has to
//! act:
//! - the owner of a job whose new checkpoint left the images over the limit
//!   gets [`Warning::CheckpointOverLimit`], once per checkpoint;
//! - administrators get [`Warning::CheckpointUsageOverLimit`] while the
//!   images are over the limit, rate-limited by the daemon.
//!
//! The header value is a comma-separated list of `kind=argument` items, e.g.
//! `checkpoint-failed=3, restore-failed=5, checkpoint-usage=1200/1000`.
//! Clients skip kinds they do not know, so new kinds can be added without a
//! new API version. Details (such as the failure reason) are not in the
//! header; they stay in the job view (`JobDetails::last_error`).

use std::fmt;

use puzuz_core::model::JobId;

/// Response header carrying pending [`Warning`]s.
pub const WARNINGS_HEADER: &str = "x-puzuz-warnings";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Warning {
    /// The last checkpoint of this job (owned by the caller) failed.
    CheckpointFailed { job: JobId },
    /// Restoring this job (owned by the caller) failed; it is still
    /// checkpointed.
    RestoreFailed { job: JobId },
    /// The new checkpoint of this job (owned by the caller) left the
    /// checkpoint images at `total_bytes`, over `limit_bytes`.
    CheckpointOverLimit {
        job: JobId,
        total_bytes: u64,
        limit_bytes: u64,
    },
    /// For administrators: the checkpoint images take `total_bytes`, over
    /// `limit_bytes`.
    CheckpointUsageOverLimit { total_bytes: u64, limit_bytes: u64 },
}

const CHECKPOINT_FAILED: &str = "checkpoint-failed";
const RESTORE_FAILED: &str = "restore-failed";
const CHECKPOINT_OVER_LIMIT: &str = "checkpoint-over-limit";
const CHECKPOINT_USAGE: &str = "checkpoint-usage";

impl Warning {
    /// The job the warning is about, if any.
    pub fn job(&self) -> Option<JobId> {
        match self {
            Warning::CheckpointFailed { job }
            | Warning::RestoreFailed { job }
            | Warning::CheckpointOverLimit { job, .. } => Some(*job),
            Warning::CheckpointUsageOverLimit { .. } => None,
        }
    }

    /// Renders warnings as a [`WARNINGS_HEADER`] value.
    pub fn encode_header(warnings: &[Warning]) -> String {
        warnings
            .iter()
            .map(|warning| match warning {
                Warning::CheckpointFailed { job } => format!("{CHECKPOINT_FAILED}={job}"),
                Warning::RestoreFailed { job } => format!("{RESTORE_FAILED}={job}"),
                Warning::CheckpointOverLimit {
                    job,
                    total_bytes,
                    limit_bytes,
                } => format!("{CHECKPOINT_OVER_LIMIT}={job}:{total_bytes}/{limit_bytes}"),
                Warning::CheckpointUsageOverLimit {
                    total_bytes,
                    limit_bytes,
                } => format!("{CHECKPOINT_USAGE}={total_bytes}/{limit_bytes}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parses a [`WARNINGS_HEADER`] value, skipping unknown or malformed
    /// items.
    pub fn parse_header(value: &str) -> Vec<Warning> {
        fn job(raw: &str) -> Option<JobId> {
            raw.parse().ok().map(JobId)
        }
        fn sizes(raw: &str) -> Option<(u64, u64)> {
            let (total, limit) = raw.split_once('/')?;
            Some((total.parse().ok()?, limit.parse().ok()?))
        }
        value
            .split(',')
            .filter_map(|item| {
                let (kind, argument) = item.trim().split_once('=')?;
                match kind {
                    CHECKPOINT_FAILED => Some(Warning::CheckpointFailed {
                        job: job(argument)?,
                    }),
                    RESTORE_FAILED => Some(Warning::RestoreFailed {
                        job: job(argument)?,
                    }),
                    CHECKPOINT_OVER_LIMIT => {
                        let (id, rest) = argument.split_once(':')?;
                        let (total_bytes, limit_bytes) = sizes(rest)?;
                        Some(Warning::CheckpointOverLimit {
                            job: job(id)?,
                            total_bytes,
                            limit_bytes,
                        })
                    }
                    CHECKPOINT_USAGE => {
                        let (total_bytes, limit_bytes) = sizes(argument)?;
                        Some(Warning::CheckpointUsageOverLimit {
                            total_bytes,
                            limit_bytes,
                        })
                    }
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
            Warning::RestoreFailed { job } => write!(
                f,
                "restoring job {job} failed; it is still checkpointed \
                 (see `puzuz log {job}` for the reason)"
            ),
            Warning::CheckpointOverLimit {
                job,
                total_bytes,
                limit_bytes,
            } => write!(
                f,
                "the checkpoint of job {job} brought checkpoint images to {}, over the limit \
                 of {}; images are not deleted automatically, so restore the job \
                 (`puzuz restore {job}`) or remove it (`puzuz remove {job}`) when you can",
                format_bytes(*total_bytes),
                format_bytes(*limit_bytes),
            ),
            Warning::CheckpointUsageOverLimit {
                total_bytes,
                limit_bytes,
            } => write!(
                f,
                "checkpoint images take {}, over checkpoint_size_limit_bytes ({}); they are \
                 not deleted automatically: find large checkpoints with `puzuz status` and \
                 restore them, or `puzuz remove` jobs that are no longer needed",
                format_bytes(*total_bytes),
                format_bytes(*limit_bytes),
            ),
        }
    }
}

/// `1.5 GiB`-style sizes for warning messages.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_skips_unknown_items() {
        let warnings = [
            Warning::CheckpointFailed { job: JobId(3) },
            Warning::RestoreFailed { job: JobId(4) },
            Warning::CheckpointFailed { job: JobId(5) },
        ];
        let header = Warning::encode_header(&warnings);
        assert_eq!(
            header,
            "checkpoint-failed=3, restore-failed=4, checkpoint-failed=5"
        );
        assert_eq!(Warning::parse_header(&header), warnings);
        assert_eq!(
            Warning::parse_header(
                "future-kind=x,checkpoint-failed=7,checkpoint-failed=no, restore-failed=, junk"
            ),
            [Warning::CheckpointFailed { job: JobId(7) }]
        );
        assert!(Warning::parse_header("").is_empty());
    }

    #[test]
    fn warnings_name_their_job() {
        let restore = Warning::RestoreFailed { job: JobId(9) };
        assert_eq!(restore.job(), Some(JobId(9)));
        assert!(restore.to_string().contains("restoring job 9 failed"));
        assert_eq!(
            Warning::CheckpointFailed { job: JobId(2) }.job(),
            Some(JobId(2))
        );
    }

    #[test]
    fn size_warnings_round_trip_and_say_what_to_do() {
        let warnings = [
            Warning::CheckpointOverLimit {
                job: JobId(4),
                total_bytes: 3 << 30,
                limit_bytes: 2 << 30,
            },
            Warning::CheckpointUsageOverLimit {
                total_bytes: 1536,
                limit_bytes: 1000,
            },
        ];
        let header = Warning::encode_header(&warnings);
        assert_eq!(
            header,
            "checkpoint-over-limit=4:3221225472/2147483648, checkpoint-usage=1536/1000"
        );
        assert_eq!(Warning::parse_header(&header), warnings);
        let malformed = "checkpoint-usage=1, checkpoint-usage=a/1, \
                         checkpoint-over-limit=4/1/2, checkpoint-over-limit=x:1/2";
        assert!(Warning::parse_header(malformed).is_empty());

        assert_eq!(warnings[0].job(), Some(JobId(4)));
        assert_eq!(warnings[1].job(), None);
        let owner = warnings[0].to_string();
        assert!(owner.contains("job 4"), "{owner}");
        assert!(owner.contains("3.0 GiB"), "{owner}");
        assert!(owner.contains("2.0 GiB"), "{owner}");
        assert!(owner.contains("puzuz remove 4"), "{owner}");
        let admin = warnings[1].to_string();
        assert!(admin.contains("1.5 KiB"), "{admin}");
        assert!(admin.contains("1000 B"), "{admin}");
        assert!(admin.contains("puzuz status"), "{admin}");
        assert!(admin.contains("puzuz remove"), "{admin}");
    }
}
