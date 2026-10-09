//! `GET /v1/jobs/{id}/log`: the raw log bytes, optionally only the last N
//! lines (`tail`) and/or followed while the job runs (`follow`).

use std::convert::Infallible;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use puzuz_core::authz;
use puzuz_core::model::{JobId, JobState};
use puzuz_core::storage::StoreOps;
use puzuz_protocol::api::{CONTENT_TYPE_LOG, ErrorCode, LogQuery};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use super::{AppState, Auth, parse_job_id};
use crate::error::{ApiResult, Failure};
use crate::state::Core;

const CHUNK: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    let path = core.log_path(info.job.owner, id);
    let start = match query.tail {
        Some(lines) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || tail_offset(&path, lines))
                .await
                .map_err(|e| Failure::internal(e.to_string()))?
                .or_else(|e| {
                    if e.kind() == io::ErrorKind::NotFound {
                        Ok(0)
                    } else {
                        Err(Failure::internal(format!("cannot read the log: {e}")))
                    }
                })?
        }
        None => 0,
    };
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    tokio::spawn(produce(core, id, path, start, query.follow, tx));
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

/// Offset where the last `lines` lines of the file start. A trailing
/// newline does not start another line.
fn tail_offset(path: &FsPath, lines: u64) -> io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if lines == 0 {
        return Ok(len);
    }
    let mut buf = vec![0u8; CHUNK];
    let mut pos = len;
    let mut found = 0;
    while pos > 0 {
        let size = (pos as usize).min(CHUNK);
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
    Ok(0)
}

/// Whether the job will not write to its log anymore (for `follow`).
async fn finished(core: &Core, id: JobId) -> bool {
    match core.read(move |s| s.get_job(id)).await {
        Ok(Some(job)) => matches!(job.state, JobState::Done { .. } | JobState::Checkpointed),
        _ => true,
    }
}

async fn produce(
    core: Arc<Core>,
    id: JobId,
    path: PathBuf,
    mut offset: u64,
    follow: bool,
    tx: mpsc::Sender<Bytes>,
) {
    let mut file: Option<tokio::fs::File> = None;
    let mut buf = vec![0u8; CHUNK];
    let mut last_round = false;
    loop {
        if file.is_none() {
            match tokio::fs::File::open(&path).await {
                Ok(mut f) => {
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
        if finished(&core, id).await {
            // One more pass picks up output written just before the exit.
            last_round = true;
            continue;
        }
        tokio::select! {
            _ = core.cancel.cancelled() => return,
            _ = tx.closed() => return,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tail_offset;

    #[test]
    fn tail_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
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
}
