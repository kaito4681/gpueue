//! `GET /v1/jobs/{id}/log`: the raw log bytes, optionally only the last N
//! lines (`tail`) and/or followed while the job runs (`follow`).
//!
//! The owner controls the log, so it may be huge or sparse. Finding where
//! the last lines start reads backwards on the blocking thread pool, which
//! the database shares: at most [`MAX_TAIL_SCAN`] bytes are scanned (the
//! output then starts there), only a few scans run at once, and a scan
//! stops when its client goes away. Followed logs count against the
//! caller's stream limit (see [`crate::limits`]) and are re-read every
//! [`POLL_INTERVAL`]; whether the job has finished is learnt from the
//! daemon's change notifications rather than by asking the database.

use std::convert::Infallible;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use puzuz_core::authz;
use puzuz_core::model::{JobId, JobState, Uid};
use puzuz_core::storage::StoreOps;
use puzuz_protocol::api::{CONTENT_TYPE_LOG, ErrorCode, LogQuery};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::{AppState, Auth, parse_job_id};
use crate::error::{ApiResult, Failure};
use crate::limits::{MAX_STREAMS_PER_UID, UidPermit};
use crate::state::{Core, DaemonEvent};

const CHUNK: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How far back `tail` looks for line starts at most.
const MAX_TAIL_SCAN: u64 = 4 * 1024 * 1024;

/// How often a followed log checks the job's state without a change
/// notification (in case notifications were missed).
const STATE_RECHECK: Duration = Duration::from_secs(30);

pub async fn job_log(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(id): Path<String>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> ApiResult<Response> {
    let Query(query) = query.map_err(|e| Failure::bad_request(e.body_text()))?;
    let id = parse_job_id(&id)?;
    let info = core.job_info(id).await?;
    if !authz::can_read_log(&caller, &info.job) {
        return Err(Failure::new(
            ErrorCode::PermissionDenied,
            format!("only the owner of job {id} and administrators may read its log"),
        ));
    }
    let permit = if query.follow {
        Some(
            core.streams
                .try_acquire(caller.uid.0)
                .ok_or_else(too_many_streams)?,
        )
    } else {
        None
    };
    // The log is opened once for the request, without trusting its
    // directory (see `crate::logfile`); one that does not exist yet is
    // opened once it does (`follow`). Fail now rather than with an empty
    // body if the log is not a log.
    let owner = info.job.owner;
    let file = match open_log(&core, owner, id).await {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(cannot_read(e)),
    };
    let (file, start) = match (file, query.tail) {
        (Some(file), Some(lines)) => {
            let (file, start) = tail_start(&core, file, lines).await?;
            (Some(file), start)
        }
        (file, _) => (file, 0),
    };
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    let file = file.map(tokio::fs::File::from_std);
    tokio::spawn(produce(
        core,
        id,
        owner,
        file,
        start,
        query.follow,
        tx,
        permit,
    ));
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), rx))
    });
    Ok((
        [(header::CONTENT_TYPE, CONTENT_TYPE_LOG)],
        Body::from_stream(stream),
    )
        .into_response())
}

/// The answer to a stream over the caller's limit.
pub fn too_many_streams() -> Failure {
    Failure::new(
        ErrorCode::TooManyRequests,
        format!(
            "too many followed logs and event streams open (at most {MAX_STREAMS_PER_UID} per \
             user); close some and try again"
        ),
    )
}

/// Where the last `lines` lines of the log `file` start (see
/// [`tail_offset`]), and the file back.
async fn tail_start(
    core: &Arc<Core>,
    mut file: std::fs::File,
    lines: u64,
) -> ApiResult<(std::fs::File, u64)> {
    // Waiting for a turn ends with the request.
    let _turn = core
        .tail_scans
        .acquire()
        .await
        .map_err(|e| Failure::internal(e.to_string()))?;
    // Cancelled when the request is dropped (the client went away), which
    // stops the scan.
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        let start = tail_offset(&mut file, lines, MAX_TAIL_SCAN, &cancel)?;
        Ok((file, start))
    })
    .await
    .map_err(|e| Failure::internal(e.to_string()))?
    .map_err(cannot_read)
}

fn cannot_read(e: io::Error) -> Failure {
    let code = if e.kind() == io::ErrorKind::PermissionDenied {
        ErrorCode::PermissionDenied
    } else {
        ErrorCode::Internal
    };
    Failure::new(code, format!("cannot read the log: {e}"))
}

/// See [`Core::open_log`].
async fn open_log(core: &Arc<Core>, owner: Uid, id: JobId) -> io::Result<std::fs::File> {
    let core = core.clone();
    tokio::task::spawn_blocking(move || core.open_log(owner, id))
        .await
        .map_err(io::Error::other)?
}

/// Offset where the last `lines` lines of the file start. A trailing
/// newline does not start another line. Looks at the last `max_scan`
/// bytes at most: if they hold fewer lines, they are where the output
/// starts. Gives up once `cancel` fires.
fn tail_offset(
    file: &mut std::fs::File,
    lines: u64,
    max_scan: u64,
    cancel: &CancellationToken,
) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if lines == 0 {
        return Ok(len);
    }
    let floor = len.saturating_sub(max_scan);
    let mut buf = vec![0u8; CHUNK];
    let mut pos = len;
    let mut found = 0;
    while pos > floor {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the client went away",
            ));
        }
        let size = ((pos - floor) as usize).min(CHUNK);
        pos -= size as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..size])?;
        for i in (0..size).rev() {
            let at = pos + i as u64;
            if buf[i] == b'\n' && at + 1 != len {
                found += 1;
                if found == lines {
                    return Ok(at + 1);
                }
            }
        }
    }
    Ok(floor)
}

/// Whether a job in `state` will not write to its log anymore.
fn is_finished(state: &JobState) -> bool {
    matches!(state, JobState::Done { .. } | JobState::Checkpointed)
}

/// Whether job `id` will not write to its log anymore (for `follow`).
async fn finished(core: &Core, id: JobId) -> bool {
    match core.read(move |s| s.get_job(id)).await {
        Ok(Some(job)) => is_finished(&job.state),
        _ => true,
    }
}

/// What a followed log learns from a change notification.
enum Notice {
    /// The job finished (or is gone).
    Finished,
    /// Unknown (notifications were missed): ask the database.
    Recheck,
    /// Nothing about this job.
    Other,
}

fn notice(event: Result<DaemonEvent, broadcast::error::RecvError>, id: JobId) -> Notice {
    match event {
        Ok(DaemonEvent::JobUpdated(info)) if info.job.id == id => {
            if is_finished(&info.job.state) {
                Notice::Finished
            } else {
                Notice::Other
            }
        }
        Ok(DaemonEvent::JobRemoved(removed)) if removed == id => Notice::Finished,
        Ok(DaemonEvent::Shutdown) => Notice::Finished,
        Ok(_) => Notice::Other,
        Err(broadcast::error::RecvError::Lagged(_)) => Notice::Recheck,
        Err(broadcast::error::RecvError::Closed) => Notice::Finished,
    }
}

/// Streams the log from `offset` on: from `file`, the log opened by
/// [`job_log`], or (`None`: there was none yet) once it exists.
#[allow(clippy::too_many_arguments)]
async fn produce(
    core: Arc<Core>,
    id: JobId,
    owner: Uid,
    file: Option<tokio::fs::File>,
    mut offset: u64,
    follow: bool,
    tx: mpsc::Sender<Bytes>,
    // Held while the stream is open.
    _permit: Option<UidPermit>,
) {
    // Subscribed before the first state check, so no change is missed.
    let mut events = core.subscribe();
    let mut file = file;
    if let Some(f) = file.as_mut()
        && f.seek(SeekFrom::Start(offset)).await.is_err()
    {
        return;
    }
    let mut buf = vec![0u8; CHUNK];
    let mut last_round = false;
    let mut check_state = true;
    let mut next_recheck = tokio::time::Instant::now() + STATE_RECHECK;
    loop {
        if file.is_none() {
            match open_log(&core, owner, id).await {
                Ok(f) => {
                    let mut f = tokio::fs::File::from_std(f);
                    if f.seek(SeekFrom::Start(offset)).await.is_err() {
                        return;
                    }
                    file = Some(f);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(job = %id, "cannot open log: {e}");
                    return;
                }
            }
        }
        if let Some(f) = file.as_mut() {
            loop {
                match f.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        offset += n as u64;
                        if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(job = %id, "cannot read log: {e}");
                        return;
                    }
                }
            }
        }
        if !follow || last_round {
            return;
        }
        if check_state || tokio::time::Instant::now() >= next_recheck {
            check_state = false;
            next_recheck = tokio::time::Instant::now() + STATE_RECHECK;
            if finished(&core, id).await {
                // One more pass picks up output written just before the exit.
                last_round = true;
                continue;
            }
        }
        let poll = tokio::time::sleep(POLL_INTERVAL);
        tokio::pin!(poll);
        loop {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = tx.closed() => return,
                _ = &mut poll => break,
                event = events.recv() => match notice(event, id) {
                    Notice::Finished => {
                        last_round = true;
                        break;
                    }
                    Notice::Recheck => {
                        check_state = true;
                        break;
                    }
                    Notice::Other => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tokio_util::sync::CancellationToken;

    use super::tail_offset;

    #[test]
    fn tail_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let never = CancellationToken::new();
        let tail_offset = |path: &std::path::Path, lines| {
            tail_offset(
                &mut std::fs::File::open(path).unwrap(),
                lines,
                1 << 20,
                &never,
            )
        };
        std::fs::write(&path, "a\nbb\nccc\n").unwrap();
        assert_eq!(tail_offset(&path, 1).unwrap(), 5);
        assert_eq!(tail_offset(&path, 2).unwrap(), 2);
        assert_eq!(tail_offset(&path, 3).unwrap(), 0);
        assert_eq!(tail_offset(&path, 10).unwrap(), 0);
        assert_eq!(tail_offset(&path, 0).unwrap(), 9);
        std::fs::write(&path, "a\nbb\nccc").unwrap();
        assert_eq!(tail_offset(&path, 1).unwrap(), 5);
        assert_eq!(tail_offset(&path, 2).unwrap(), 2);
    }

    /// A huge sparse log (no newlines at all) is not read through: the scan
    /// stops after `max_scan` bytes, and the output starts there.
    #[test]
    fn tail_scan_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let file = std::fs::File::create(&path).unwrap();
        let len = 1u64 << 36;
        file.set_len(len).unwrap();
        let never = CancellationToken::new();
        let started = Instant::now();
        let at = tail_offset(
            &mut std::fs::File::open(&path).unwrap(),
            10,
            4 << 20,
            &never,
        )
        .unwrap();
        assert_eq!(at, len - (4 << 20));
        assert!(started.elapsed().as_secs() < 5);
        // Lines found within the window are honoured.
        std::fs::write(&path, b"x\n".repeat(100)).unwrap();
        let tail = |lines| tail_offset(&mut std::fs::File::open(&path).unwrap(), lines, 10, &never);
        assert_eq!(tail(2).unwrap(), 196);
        assert_eq!(tail(3).unwrap(), 194);
        assert_eq!(tail(10).unwrap(), 190);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            tail_offset(
                &mut std::fs::File::open(&path).unwrap(),
                3,
                1 << 20,
                &cancelled
            )
            .is_err()
        );
    }
}
