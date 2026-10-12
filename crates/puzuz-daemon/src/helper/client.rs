//! [`HelperClient`]: the real connection to `puzuz-helper`.
//!
//! One background task keeps a single connection open and reconnects with
//! a capped exponential backoff. After each successful Hello it asks the
//! helper for its live jobs and reports them with
//! [`HelperNotice::Connected`], after every event the helper sent before
//! answering (that is, its replay of unacknowledged events).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use puzuz_core::model::JobId;
use puzuz_protocol::helper::{
    CheckpointOptions, DaemonMessage, EventSeq, Hello, HelperMessage, LineBuffer, Request,
    RequestEnvelope, RequestId, ResponseResult, RestoreRequest, RunningJob, SpawnRequest,
    decode_line, encode_line,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Helper, HelperError, HelperNotice, HelperResult, HelperState};

const MIN_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Long operations answer `accepted` right away, so every request should be
/// answered quickly.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Connection to the helper's Unix socket. Cheap to clone.
#[derive(Clone)]
pub struct HelperClient {
    inner: Arc<Inner>,
}

struct Inner {
    socket: PathBuf,
    next_id: AtomicU64,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<ResponseResult>>>,
    state: Mutex<HelperState>,
    notices: Mutex<Option<mpsc::UnboundedSender<HelperNotice>>>,
    task: Mutex<Option<CancellationToken>>,
}

impl HelperClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                socket: socket.into(),
                next_id: AtomicU64::new(1),
                writer: tokio::sync::Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                state: Mutex::new(HelperState::default()),
                notices: Mutex::new(None),
                task: Mutex::new(None),
            }),
        }
    }
}

impl Inner {
    fn notify(&self, notice: HelperNotice) {
        if let Some(tx) = self.notices.lock().unwrap().as_ref() {
            let _ = tx.send(notice);
        }
    }

    async fn request(&self, request: Request) -> HelperResult<ResponseResult> {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let line = encode_line(&DaemonMessage::Request(RequestEnvelope { id, request }))
            .map_err(|e| HelperError::Protocol(e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut writer = self.writer.lock().await;
            let result = match writer.as_mut() {
                None => Err(HelperError::NotConnected),
                Some(w) => w
                    .write_all(&line)
                    .await
                    .map_err(|_| HelperError::ConnectionLost),
            };
            if let Err(e) = result {
                self.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(ResponseResult::Error { code, message })) => {
                Err(HelperError::Rejected { code, message })
            }
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(HelperError::ConnectionLost),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(HelperError::Timeout)
            }
        }
    }

    /// Handles a line [`decode_incoming`] could not make sense of:
    /// - an event is passed on as [`HelperNotice::UndecodableEvent`], so the
    ///   notice loop acknowledges it in order and the helper stops
    ///   replaying it;
    /// - a response fails its request as if the connection had been lost
    ///   (whether it took effect is unknown);
    /// - anything else is logged and ignored.
    fn skip_undecodable(&self, value: Option<serde_json::Value>, error: &str) {
        let field = |name: &str| {
            value
                .as_ref()
                .and_then(|v| v.get(name))
                .and_then(serde_json::Value::as_u64)
        };
        let kind = value
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(serde_json::Value::as_str);
        match (kind, field("seq"), field("id")) {
            (Some("event"), Some(seq), _) => self.notify(HelperNotice::UndecodableEvent {
                seq: EventSeq(seq),
                error: error.to_owned(),
            }),
            (Some("response"), _, Some(id)) => {
                tracing::warn!(request = id, "cannot decode the helper's answer: {error}");
                // Dropping the sender fails the request with ConnectionLost.
                self.pending.lock().unwrap().remove(&RequestId(id));
            }
            _ => tracing::warn!("ignoring a message from the helper: {error}"),
        }
    }

    async fn expect_ok(&self, request: Request) -> HelperResult<()> {
        match self.request(request).await? {
            ResponseResult::Ok | ResponseResult::Accepted => Ok(()),
            other => Err(HelperError::Protocol(format!("{other:?}"))),
        }
    }
}

/// A line read from the helper.
#[derive(Debug)]
enum Incoming {
    Message(HelperMessage),
    /// Valid JSON that is not a message this daemon understands (e.g. an
    /// event a newer helper added), or not JSON at all (`value` is `None`).
    Undecodable {
        value: Option<serde_json::Value>,
        error: String,
    },
}

/// Decodes one line. Undecodable lines are returned as such rather than
/// failing, so that one bad line does not break the connection (the helper
/// would replay an undecodable event after every reconnect).
fn decode_incoming(line: &[u8]) -> Incoming {
    let value: serde_json::Value = match decode_line(line) {
        Ok(value) => value,
        Err(e) => {
            return Incoming::Undecodable {
                value: None,
                error: e.to_string(),
            };
        }
    };
    match serde_json::from_value(value.clone()) {
        Ok(message) => Incoming::Message(message),
        Err(e) => Incoming::Undecodable {
            value: Some(value),
            error: e.to_string(),
        },
    }
}

async fn read_message(
    reader: &mut OwnedReadHalf,
    buf: &mut LineBuffer,
) -> Result<Option<Incoming>, String> {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if let Some(line) = buf.next_line().map_err(|e| e.to_string())? {
            if line.is_empty() {
                continue;
            }
            return Ok(Some(decode_incoming(&line)));
        }
        let n = reader.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(&chunk[..n]);
    }
}

/// Runs one connection until it breaks or `cancel` fires.
async fn session(inner: &Arc<Inner>, stream: UnixStream, cancel: &CancellationToken) -> String {
    let (mut reader, mut writer) = stream.into_split();
    let hello = match encode_line(&DaemonMessage::Hello(Hello::current())) {
        Ok(line) => line,
        Err(e) => return e.to_string(),
    };
    if let Err(e) = writer.write_all(&hello).await {
        return format!("cannot send hello: {e}");
    }
    let mut buf = LineBuffer::new();
    let theirs =
        match tokio::time::timeout(HELLO_TIMEOUT, read_message(&mut reader, &mut buf)).await {
            Ok(Ok(Some(Incoming::Message(HelperMessage::Hello(hello))))) => hello,
            Ok(Ok(Some(other))) => return format!("expected hello, got {other:?}"),
            Ok(Ok(None)) => return "connection closed before hello".to_owned(),
            Ok(Err(e)) => return e,
            Err(_) => return "no hello from the helper".to_owned(),
        };
    if !Hello::current().is_compatible(&theirs) {
        return format!(
            "incompatible helper protocol {} (version {}); this daemon speaks {}",
            theirs.protocol_version,
            theirs.binary_version,
            puzuz_protocol::helper::PROTOCOL_VERSION
        );
    }
    *inner.writer.lock().await = Some(writer);
    *inner.state.lock().unwrap() = HelperState {
        connected: true,
        version: Some(theirs.binary_version.clone()),
    };
    tracing::info!(version = %theirs.binary_version, "connected to the puzuz helper");

    // Reconcile once the replayed events have been read (the helper sends
    // them before answering requests).
    let reconcile = {
        let inner = inner.clone();
        let version = theirs.binary_version;
        tokio::spawn(async move {
            match inner.request(Request::ListRunning).await {
                Ok(ResponseResult::Running { jobs }) => {
                    inner.notify(HelperNotice::Connected {
                        version,
                        running: jobs,
                    });
                }
                Ok(other) => tracing::warn!(?other, "unexpected answer to list_running"),
                Err(e) => tracing::warn!("cannot list the helper's jobs: {e}"),
            }
        })
    };

    let reason = loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => break "closed".to_owned(),
            message = read_message(&mut reader, &mut buf) => message,
        };
        match message {
            Ok(Some(Incoming::Message(HelperMessage::Response(response)))) => {
                if let Some(tx) = inner.pending.lock().unwrap().remove(&response.id) {
                    let _ = tx.send(response.result);
                }
            }
            Ok(Some(Incoming::Message(HelperMessage::Event(event)))) => {
                inner.notify(HelperNotice::Event(event))
            }
            Ok(Some(Incoming::Message(HelperMessage::Hello(_)))) => {
                tracing::warn!("ignoring a repeated hello")
            }
            Ok(Some(Incoming::Undecodable { value, error })) => {
                inner.skip_undecodable(value, &error)
            }
            Ok(None) => break "connection closed by the helper".to_owned(),
            Err(e) => break e,
        }
    };
    reconcile.abort();
    reason
}

async fn connection_loop(inner: Arc<Inner>, cancel: CancellationToken) {
    let mut backoff = MIN_BACKOFF;
    let mut warned = false;
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => return,
            stream = UnixStream::connect(&inner.socket) => stream,
        };
        match stream {
            Ok(stream) => {
                backoff = MIN_BACKOFF;
                warned = false;
                let reason = session(&inner, stream, &cancel).await;
                let was_connected = std::mem::take(&mut *inner.state.lock().unwrap()).connected;
                *inner.writer.lock().await = None;
                // Dropping the senders fails every waiting request.
                inner.pending.lock().unwrap().clear();
                if was_connected {
                    tracing::warn!("disconnected from the puzuz helper: {reason}");
                    inner.notify(HelperNotice::Disconnected);
                } else if !cancel.is_cancelled() {
                    tracing::warn!("cannot talk to the puzuz helper: {reason}");
                }
            }
            Err(e) => {
                if !warned {
                    tracing::warn!(
                        socket = %inner.socket.display(),
                        "cannot connect to the puzuz helper (will keep retrying): {e}"
                    );
                    warned = true;
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

impl Helper for HelperClient {
    fn connect(&self) -> mpsc::UnboundedReceiver<HelperNotice> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.inner.notices.lock().unwrap() = Some(tx);
        let mut task = self.inner.task.lock().unwrap();
        if task.is_none() {
            let cancel = CancellationToken::new();
            tokio::spawn(connection_loop(self.inner.clone(), cancel.clone()));
            *task = Some(cancel);
        }
        rx
    }

    fn close(&self) {
        if let Some(cancel) = self.inner.task.lock().unwrap().take() {
            cancel.cancel();
        }
        *self.inner.notices.lock().unwrap() = None;
    }

    fn state(&self) -> HelperState {
        self.inner.state.lock().unwrap().clone()
    }

    fn spawn(&self, request: SpawnRequest) -> BoxFuture<'_, HelperResult<u32>> {
        async move {
            match self.inner.request(Request::Spawn(request)).await? {
                ResponseResult::Spawned { pid } => Ok(pid),
                other => Err(HelperError::Protocol(format!("{other:?}"))),
            }
        }
        .boxed()
    }

    fn signal(&self, job: JobId, signal: i32) -> BoxFuture<'_, HelperResult<()>> {
        self.inner
            .expect_ok(Request::Signal { job, signal })
            .boxed()
    }

    fn checkpoint(
        &self,
        job: JobId,
        image_dir: PathBuf,
        options: CheckpointOptions,
    ) -> BoxFuture<'_, HelperResult<()>> {
        self.inner
            .expect_ok(Request::Checkpoint {
                job,
                image_dir,
                options,
            })
            .boxed()
    }

    fn restore(&self, request: RestoreRequest) -> BoxFuture<'_, HelperResult<()>> {
        self.inner.expect_ok(Request::Restore(request)).boxed()
    }

    fn delete_checkpoint(&self, job: JobId, image_dir: PathBuf) -> BoxFuture<'_, HelperResult<()>> {
        self.inner
            .expect_ok(Request::DeleteCheckpoint { job, image_dir })
            .boxed()
    }

    fn list_running(&self) -> BoxFuture<'_, HelperResult<Vec<RunningJob>>> {
        async move {
            match self.inner.request(Request::ListRunning).await? {
                ResponseResult::Running { jobs } => Ok(jobs),
                other => Err(HelperError::Protocol(format!("{other:?}"))),
            }
        }
        .boxed()
    }

    fn ack_events(&self, up_to: EventSeq) -> BoxFuture<'_, HelperResult<()>> {
        self.inner.expect_ok(Request::AckEvents { up_to }).boxed()
    }
}

#[cfg(test)]
mod tests {
    use puzuz_protocol::helper::{
        Event, EventEnvelope, ExitStatus, HelperErrorCode, Response, RunningPhase,
    };
    use tokio::net::UnixListener;

    use super::*;

    /// The line the helper sends for `event`.
    fn event_line(event: EventEnvelope) -> Vec<u8> {
        encode_line(&HelperMessage::Event(event)).unwrap()
    }

    /// Accepts one connection, does the Hello, sends the `replay` lines,
    /// then answers requests until the client goes away.
    async fn serve_once(listener: &UnixListener, replay: Vec<Vec<u8>>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let mut buf = LineBuffer::new();
        let mut chunk = [0u8; 4096];
        let mut next = async |reader: &mut OwnedReadHalf, buf: &mut LineBuffer| loop {
            if let Some(line) = buf.next_line().unwrap() {
                return Some(decode_line::<DaemonMessage>(&line).unwrap());
            }
            let n = reader.read(&mut chunk).await.unwrap();
            if n == 0 {
                return None;
            }
            buf.push(&chunk[..n]);
        };
        assert!(matches!(
            next(&mut reader, &mut buf).await,
            Some(DaemonMessage::Hello(_))
        ));
        let send = |m: HelperMessage| encode_line(&m).unwrap();
        writer
            .write_all(&send(HelperMessage::Hello(Hello::current())))
            .await
            .unwrap();
        for line in replay {
            writer.write_all(&line).await.unwrap();
        }
        while let Some(DaemonMessage::Request(envelope)) = next(&mut reader, &mut buf).await {
            let result = match envelope.request {
                Request::ListRunning => ResponseResult::Running {
                    jobs: vec![RunningJob {
                        job: JobId(3),
                        pid: Some(42),
                        phase: RunningPhase::Running,
                    }],
                },
                Request::Spawn(_) => ResponseResult::Spawned { pid: 7 },
                Request::Signal { .. } => ResponseResult::Error {
                    code: HelperErrorCode::JobNotFound,
                    message: "no such job".to_owned(),
                },
                _ => ResponseResult::Ok,
            };
            writer
                .write_all(&send(HelperMessage::Response(Response {
                    id: envelope.id,
                    result,
                })))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn talks_to_a_helper_and_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let client = HelperClient::new(&socket);
        let mut notices = client.connect();
        assert!(!client.state().connected);
        assert_eq!(
            client.signal(JobId(1), 15).await,
            Err(HelperError::NotConnected)
        );

        let listener = UnixListener::bind(&socket).unwrap();
        let event = EventEnvelope {
            seq: EventSeq(5),
            event: Event::JobExited {
                job: JobId(1),
                status: ExitStatus::Exited { code: 0 },
            },
        };
        let server = tokio::spawn(async move {
            serve_once(&listener, vec![event_line(event)]).await;
            listener
        });

        // The replayed event comes before Connected.
        assert!(matches!(
            notices.recv().await.unwrap(),
            HelperNotice::Event(EventEnvelope {
                seq: EventSeq(5),
                ..
            })
        ));
        match notices.recv().await.unwrap() {
            HelperNotice::Connected { running, .. } => assert_eq!(running[0].job, JobId(3)),
            other => panic!("{other:?}"),
        }
        assert!(client.state().connected);

        let spawn = SpawnRequest {
            job: JobId(9),
            uid: puzuz_core::model::Uid(1000),
            gid: puzuz_protocol::helper::Gid(1000),
            groups: None,
            cwd: "/".into(),
            command: "true".to_owned(),
            shell: vec!["sh".to_owned(), "-c".to_owned()],
            envs: Default::default(),
            gpus: vec![],
            log_path: "/tmp/x.log".into(),
            no_checkpoint: false,
        };
        assert_eq!(client.spawn(spawn).await, Ok(7));
        assert!(matches!(
            client.signal(JobId(1), 15).await,
            Err(HelperError::Rejected {
                code: HelperErrorCode::JobNotFound,
                ..
            })
        ));
        assert_eq!(client.ack_events(EventSeq(5)).await, Ok(()));

        // Drop the server side: the client notices and reconnects.
        client
            .inner
            .writer
            .lock()
            .await
            .as_mut()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        let listener = server.await.unwrap();
        assert_eq!(notices.recv().await.unwrap(), HelperNotice::Disconnected);
        let server = tokio::spawn(async move { serve_once(&listener, Vec::new()).await });
        assert!(matches!(
            notices.recv().await.unwrap(),
            HelperNotice::Connected { .. }
        ));
        client.close();
        server.abort();
    }

    /// A line the daemon cannot decode does not break the connection: an
    /// event is passed on to be acknowledged, anything else is skipped.
    #[tokio::test]
    async fn undecodable_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let client = HelperClient::new(&socket);
        let mut notices = client.connect();
        let listener = UnixListener::bind(&socket).unwrap();
        let lines = [
            &br#"{"type":"event","seq":6,"event":"from_the_future","job":1}"#[..],
            b"not json at all",
            br#"{"type":"gossip"}"#,
        ]
        .map(|line| [line, b"\n"].concat());
        let good = EventEnvelope {
            seq: EventSeq(7),
            event: Event::JobExited {
                job: JobId(1),
                status: ExitStatus::Exited { code: 0 },
            },
        };
        let mut replay = lines.to_vec();
        replay.push(event_line(good.clone()));
        let server = tokio::spawn(async move { serve_once(&listener, replay).await });

        match notices.recv().await.unwrap() {
            HelperNotice::UndecodableEvent { seq, error } => {
                assert_eq!(seq, EventSeq(6));
                assert!(error.contains("from_the_future"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(notices.recv().await.unwrap(), HelperNotice::Event(good));
        assert!(matches!(
            notices.recv().await.unwrap(),
            HelperNotice::Connected { .. }
        ));
        assert_eq!(client.ack_events(EventSeq(7)).await, Ok(()));
        assert!(client.state().connected);
        client.close();
        server.abort();
    }
}
