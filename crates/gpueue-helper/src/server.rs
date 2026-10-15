//! The daemon-facing Unix socket: peer uid check, Hello, request dispatch
//! and event delivery with replay.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpueue_protocol::helper::{
    DaemonMessage, EventEnvelope, EventSeq, Hello, HelperErrorCode, HelperMessage, LineBuffer,
    RequestId, Response, ResponseResult, decode_line, encode_line,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch};

use crate::engine::Helper;
use crate::host::HostProbe;
use crate::launcher::Launcher;

/// How long a client may take to send its Hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections may be between `accept` and the end of their
/// Hello at once; further ones are closed right away. Only the daemon's
/// uid gets past the peer check, and each such connection may buffer up to
/// a [`gpueue_protocol::helper::MAX_LINE_LEN`] line.
pub const MAX_HANDSHAKES: usize = 4;
/// Requests of a session that may be in progress at once; the session reads
/// no further request until one of them is answered.
pub const MAX_INFLIGHT_REQUESTS: usize = 64;
/// Messages queued for a session's writer. When the daemon does not read,
/// the senders wait (and with them the request reader, see
/// [`MAX_INFLIGHT_REQUESTS`]), so memory stays bounded.
pub const OUTGOING_QUEUE: usize = 256;

/// Bind the socket: replace a stale one, create it mode 0600 and hand it to
/// the daemon's uid (the peer uid is checked again per connection).
///
/// The socket's directory (`/run/gpueue`) is writable by the daemon, so no
/// path below it is followed with root privileges: the socket is created in
/// a fresh private (0700, root-owned) sibling directory that is only used
/// through a file descriptor (the daemon may rename or replace the entry,
/// but not what the descriptor refers to), set up there, and renamed into
/// place relative to a descriptor of the socket's directory (`rename`
/// replaces whatever is at the target without following it).
pub fn bind(path: &Path, daemon_uid: u32) -> io::Result<UnixListener> {
    use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags};
    use std::os::fd::AsRawFd;

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no name"))?;
    std::fs::create_dir_all(parent)?;
    let dirflags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent_fd = rustix::fs::open(parent, dirflags, Mode::empty())?;
    // A fresh name: whatever the daemon left at an older one is not
    // touched.
    let mut attempt = 0u32;
    let private = loop {
        let candidate = format!(".helper-sock.{}.{attempt}", std::process::id());
        match rustix::fs::mkdirat(&parent_fd, candidate.as_str(), Mode::RWXU) {
            Ok(()) => break candidate,
            Err(rustix::io::Errno::EXIST) if attempt < 1000 => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let me = nix::unistd::geteuid().as_raw();
    let result = (|| -> io::Result<UnixListener> {
        let dir = rustix::fs::openat2(
            &parent_fd,
            private.as_str(),
            dirflags,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS | ResolveFlags::BENEATH,
        )?;
        // The daemon could have swapped the entry for a directory of its
        // own between mkdirat and openat2; ours is ours and private.
        let st = rustix::fs::fstat(&dir)?;
        if st.st_uid != me || st.st_mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{}/{private} was replaced while binding the socket",
                    parent.display()
                ),
            ));
        }
        // Only we can create entries in `dir`: nothing in it is a symlink.
        let staged = format!("/proc/self/fd/{}/sock", dir.as_raw_fd());
        let listener = UnixListener::bind(&staged)?;
        rustix::fs::chmodat(&dir, "sock", Mode::from_raw_mode(0o600), AtFlags::empty())?;
        if me != daemon_uid {
            rustix::fs::chownat(
                &dir,
                "sock",
                Some(rustix::fs::Uid::from_raw(daemon_uid)),
                None,
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
        }
        rustix::fs::renameat(&dir, "sock", &parent_fd, name)?;
        Ok(listener)
    })();
    // Our empty directory (or, if the daemon swapped it, whatever empty
    // directory now has that name).
    let _ = rustix::fs::unlinkat(&parent_fd, private.as_str(), AtFlags::REMOVEDIR);
    result
}

/// The current session, so that a new one can end it.
#[derive(Default)]
struct Sessions {
    current: std::sync::Mutex<(u64, Option<watch::Sender<bool>>)>,
}

impl Sessions {
    /// Registers a new session and ends the previous one. Returns the new
    /// session's number and a receiver that fires when it is replaced.
    fn begin(&self) -> (u64, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = cur.1.replace(tx) {
            let _ = old.send(true);
        }
        cur.0 += 1;
        (cur.0, rx)
    }

    fn end(&self, id: u64) {
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if cur.0 == id {
            cur.1 = None;
        }
    }
}

/// Accept connections until `stop` fires.
///
/// The daemon keeps a single connection. A new session (a connection that
/// completed its Hello) replaces the previous one, which is closed, and at
/// most [`MAX_HANDSHAKES`] connections may be waiting for their Hello, so a
/// compromised daemon user cannot make root hold many connections and
/// their buffers.
pub async fn serve<L: Launcher, H: HostProbe>(
    helper: Arc<Helper<L, H>>,
    listener: UnixListener,
    mut stop: watch::Receiver<bool>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let sessions = Arc::new(Sessions::default());
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                        eprintln!(
                            "gpueue-helper: more than {MAX_HANDSHAKES} connections waiting for \
                             their hello; closing a new one"
                        );
                        continue;
                    };
                    let helper = helper.clone();
                    let stop = stop.clone();
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let mut session = None;
                        let res = handle_connection(helper, stream, stop, permit, || {
                            let (id, replaced) = sessions.begin();
                            session = Some(id);
                            replaced
                        })
                        .await;
                        if let Some(id) = session {
                            sessions.end(id);
                        }
                        if let Err(e) = res {
                            eprintln!("gpueue-helper: connection: {e}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("gpueue-helper: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = stop.changed() => return,
        }
    }
}

/// The next non-blank line, or `None` at EOF.
async fn read_line(
    stream: &mut tokio::net::unix::OwnedReadHalf,
    buf: &mut LineBuffer,
) -> io::Result<Option<Vec<u8>>> {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if let Some(line) = buf.next_line().map_err(io::Error::other)? {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Ok(Some(line));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(&chunk[..n]);
    }
}

/// What can be made of a line that is not a valid [`DaemonMessage`]: a
/// request (e.g. an operation of a newer daemon) gets an error response with
/// its id; anything else ends the connection.
#[derive(Deserialize)]
struct PartialRequest {
    #[serde(rename = "type")]
    kind: String,
    id: RequestId,
}

/// Decodes a request line. `Err(Some(response))`: an undecodable request,
/// to be answered; `Err(None)`: not a message at all.
pub fn decode_message(line: &[u8]) -> Result<DaemonMessage, (Option<Response>, String)> {
    match decode_line::<DaemonMessage>(line) {
        Ok(m) => Ok(m),
        Err(e) => {
            let reply = decode_line::<PartialRequest>(line)
                .ok()
                .filter(|p| p.kind == "request")
                .map(|p| Response {
                    id: p.id,
                    result: ResponseResult::Error {
                        code: HelperErrorCode::InvalidRequest,
                        message: format!("unsupported or malformed request: {e}"),
                    },
                });
            Err((reply, e.to_string()))
        }
    }
}

/// Serve one daemon connection. `on_hello` is called once the Hello has
/// been exchanged; the session ends when the receiver it returns fires (a
/// newer session replaced it). `handshake` is held until then.
async fn handle_connection<L: Launcher, H: HostProbe>(
    helper: Arc<Helper<L, H>>,
    stream: UnixStream,
    mut stop: watch::Receiver<bool>,
    handshake: OwnedSemaphorePermit,
    on_hello: impl FnOnce() -> watch::Receiver<bool>,
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

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_line(&mut rd, &mut buf)).await {
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no hello")),
        Ok(r) => r?,
    };
    let Some(Ok(DaemonMessage::Hello(theirs))) = hello.map(|l| decode_line::<DaemonMessage>(&l))
    else {
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
    let mut replaced = on_hello();
    drop(handshake);

    // Writer task: everything sent to the daemon goes through `out`.
    let (out, mut out_rx) = mpsc::channel::<HelperMessage>(OUTGOING_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let line = match encode_line(&msg) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("gpueue-helper: encoding {msg:?}: {e}");
                    continue;
                }
            };
            if wr.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Events: queue every unacknowledged one *before* reading the first
    // request, so the replay always precedes any response (the daemon
    // reconciles with `list_running` and relies on having seen them).
    // Then follow live ones.
    let (pending, rx) = helper.events().subscribe();
    let mut last = EventSeq(0);
    for env in pending {
        last = env.seq;
        if out.send(HelperMessage::Event(env)).await.is_err() {
            return Ok(());
        }
    }
    let events_task = tokio::spawn(forward_events(helper.clone(), last, rx, out.clone()));

    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_REQUESTS));
    let result = loop {
        let line = tokio::select! {
            l = read_line(&mut rd, &mut buf) => l,
            _ = stop.changed() => break Ok(()),
            _ = replaced.changed() => {
                eprintln!("gpueue-helper: a new daemon connection replaces this one");
                break Ok(());
            }
        };
        let line = match line {
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
            Ok(Some(l)) => l,
        };
        match decode_message(&line) {
            Err((Some(reply), reason)) => {
                eprintln!("gpueue-helper: rejecting request {}: {reason}", reply.id.0);
                if out.send(HelperMessage::Response(reply)).await.is_err() {
                    break Ok(());
                }
            }
            Err((None, reason)) => {
                break Err(io::Error::new(io::ErrorKind::InvalidData, reason));
            }
            Ok(DaemonMessage::Hello(_)) => {
                break Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate hello",
                ));
            }
            Ok(DaemonMessage::Request(env)) => {
                let Ok(permit) = inflight.clone().acquire_owned().await else {
                    break Ok(());
                };
                let helper = helper.clone();
                let out = out.clone();
                tokio::spawn(async move {
                    let result = helper.handle(env.request).await;
                    let _ = out
                        .send(HelperMessage::Response(Response { id: env.id, result }))
                        .await;
                    drop(permit);
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
    mut last: EventSeq,
    mut rx: broadcast::Receiver<EventEnvelope>,
    out: mpsc::Sender<HelperMessage>,
) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undecodable_requests_get_an_error_with_their_id() {
        let line = br#"{"type":"request","id":7,"op":"frobnicate","job":1}"#;
        match decode_message(line) {
            Err((Some(r), _)) => {
                assert_eq!(r.id, RequestId(7));
                assert!(matches!(
                    r.result,
                    ResponseResult::Error {
                        code: HelperErrorCode::InvalidRequest,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
        // Not a request, or no id: nothing to answer.
        assert!(matches!(decode_message(b"garbage"), Err((None, _))));
        assert!(matches!(
            decode_message(br#"{"type":"request","op":"x"}"#),
            Err((None, _))
        ));
        assert!(matches!(
            decode_message(br#"{"type":"hello","id":1}"#),
            Err((None, _))
        ));
    }

    #[test]
    fn a_new_session_replaces_the_old_one() {
        let s = Sessions::default();
        let (a, rx_a) = s.begin();
        assert!(!*rx_a.borrow());
        let (b, rx_b) = s.begin();
        assert!(*rx_a.borrow(), "the first session was told to end");
        assert!(!*rx_b.borrow());
        // The old session ending does not unregister the new one.
        s.end(a);
        let (_, _rx_c) = s.begin();
        assert!(*rx_b.borrow());
        let _ = b;
    }

    #[tokio::test]
    async fn bind_does_not_follow_a_swapped_staging_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let me = nix::unistd::geteuid().as_raw();
        // Stale staging dirs of an earlier run (or planted ones) are left
        // alone and do not get in the way.
        let planted = tmp
            .path()
            .join(format!(".helper-sock.{}.0", std::process::id()));
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("sock"), b"x").unwrap();
        std::fs::set_permissions(victim.join("sock"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let sock = tmp.path().join("helper.sock");
        let _l = bind(&sock, me).unwrap();
        let m = std::fs::symlink_metadata(&sock).unwrap();
        assert!(std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()));
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(victim.join("sock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "the file behind the planted symlink was not touched"
        );
        assert!(std::fs::symlink_metadata(&planted).is_ok());
        // Only the socket and our own files are left.
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".helper-sock") && !n.ends_with(".0"))
            .collect();
        assert!(names.is_empty(), "staging dirs left: {names:?}");
        // Binding again replaces the socket.
        drop(_l);
        let _l = bind(&sock, me).unwrap();
    }
}
