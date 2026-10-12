//! Minimal `sd_notify(3)`: one datagram to `$NOTIFY_SOCKET`.

use std::io;
use std::os::unix::net::{SocketAddr, UnixDatagram};

/// Send `state` (e.g. `READY=1`) to the service manager. A missing
/// `NOTIFY_SOCKET` (not started by systemd) is not an error.
pub fn notify(state: &str) -> io::Result<()> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let sock = UnixDatagram::unbound()?;
    let bytes = path.as_encoded_bytes();
    if let Some(name) = bytes.strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;
        let addr = SocketAddr::from_abstract_name(name)?;
        sock.send_to_addr(state.as_bytes(), &addr)?;
    } else {
        sock.send_to(state.as_bytes(), path)?;
    }
    Ok(())
}

/// Ask for more stop time: `EXTEND_TIMEOUT_USEC` (systemd ≥ 236).
pub fn extend_timeout(by: std::time::Duration) {
    let _ = notify(&format!("EXTEND_TIMEOUT_USEC={}", by.as_micros()));
}
