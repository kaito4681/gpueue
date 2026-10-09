//! The daemon-facing Unix socket: peer uid check, Hello, request dispatch
//! and event delivery with replay.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use puzuz_protocol::helper::{
    DaemonMessage, EventEnvelope, EventSeq, Hello, HelperMessage, LineBuffer, Response,
    decode_line, encode_line,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};

use crate::engine::Helper;
use crate::host::HostProbe;
use crate::launcher::Launcher;

/// How long a client may take to send its Hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind the socket: replace a stale one, then restrict it to its owner and
/// hand it to the daemon's uid (the peer uid is checked again per
/// connection).
pub fn bind(path: &Path, daemon_uid: u32) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if nix::unistd::geteuid().as_raw() != daemon_uid {
        std::os::unix::fs::chown(path, Some(daemon_uid), None)?;
    }
    Ok(listener)
}

/// Accept connections until `stop` fires.
pub async fn serve<L: Launcher, H: HostProbe>(
    helper: Arc<Helper<L, H>>,
    listener: UnixListener,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let helper = helper.clone();
                    let stop = stop.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(helper, stream, stop).await {
                            eprintln!("puzuz-helper: connection: {e}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("puzuz-helper: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = stop.changed() => return,
        }
    }
}

async fn read_message(
    stream: &mut tokio::net::unix::OwnedReadHalf,
    buf: &mut LineBuffer,
) -> io::Result<Option<DaemonMessage>> {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if let Some(line) = buf.next_line().map_err(io::Error::other)? {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return decode_line(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(&chunk[..n]);
    }
}

/// Serve one daemon connection.
pub async fn handle_connection<L: Launcher, H: HostProbe>(
    helper: Arc<Helper<L, H>>,
    stream: UnixStream,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let cred = stream.peer_cred()?;
    if cred.uid() != helper.config().daemon_uid {
        // Say nothing to strangers.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("rejected connection from uid {}", cred.uid()),
        ));
    }
    let (mut rd, mut wr) = stream.into_split();
    let mut buf = LineBuffer::new();

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_message(&mut rd, &mut buf)).await {
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no hello")),
        Ok(r) => r?,
    };
    let Some(DaemonMessage::Hello(theirs)) = hello else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected hello"));
    };
    let ours = Hello::current();
    wr.write_all(&encode_line(&HelperMessage::Hello(ours.clone())).map_err(io::Error::other)?)
        .await?;
    if !ours.is_compatible(&theirs) {
        let _ = wr.shutdown().await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "incompatible daemon: protocol {} (binary {}), ours {}",
                theirs.protocol_version, theirs.binary_version, ours.protocol_version
            ),
        ));
    }

    // Writer task: everything sent to the daemon goes through `out`.
    let (out, mut out_rx) = mpsc::channel::<HelperMessage>(256);
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let line = match encode_line(&msg) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("puzuz-helper: encoding {msg:?}: {e}");
                    continue;
                }
            };
            if wr.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Events: replay the unacknowledged ones, then follow live ones.
    let (pending, rx) = helper.events().subscribe();
    let events_task = tokio::spawn(forward_events(helper.clone(), pending, rx, out.clone()));

    let result = loop {
        let msg = tokio::select! {
            m = read_message(&mut rd, &mut buf) => m,
            _ = stop.changed() => break Ok(()),
        };
        match msg {
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
            Ok(Some(DaemonMessage::Hello(_))) => {
                break Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate hello",
                ));
            }
            Ok(Some(DaemonMessage::Request(env))) => {
                let helper = helper.clone();
                let out = out.clone();
                tokio::spawn(async move {
                    let result = helper.handle(env.request).await;
                    let _ = out
                        .send(HelperMessage::Response(Response { id: env.id, result }))
                        .await;
                });
            }
        }
    };
    events_task.abort();
    drop(out);
    // Let in-flight responses drain; the writer ends when every sender is
    // gone (request tasks hold clones).
    let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    result
}

async fn forward_events<L: Launcher, H: HostProbe>(
    helper: Arc<Helper<L, H>>,
    pending: Vec<EventEnvelope>,
    mut rx: broadcast::Receiver<EventEnvelope>,
    out: mpsc::Sender<HelperMessage>,
) {
    let mut last = EventSeq(0);
    for env in pending {
        last = env.seq;
        if out.send(HelperMessage::Event(env)).await.is_err() {
            return;
        }
    }
    loop {
        let batch = match rx.recv().await {
            Ok(env) => vec![env],
            // Fell behind: re-read from the persistent queue.
            Err(broadcast::error::RecvError::Lagged(_)) => helper.events().pending_after(last),
            Err(broadcast::error::RecvError::Closed) => return,
        };
        for env in batch {
            if env.seq <= last {
                continue;
            }
            last = env.seq;
            if out.send(HelperMessage::Event(env)).await.is_err() {
                return;
            }
        }
    }
}
