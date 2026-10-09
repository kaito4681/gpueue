//! Newline-delimited JSON framing: one message per line, `\n`-terminated.
//!
//! Compact `serde_json` output never contains a raw newline (newlines inside
//! strings are escaped), so a line is always exactly one message.

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Longest accepted line, excluding the newline. Generous because spawn
/// requests carry the submitting client's whole environment.
pub const MAX_LINE_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    /// The message could not be serialized or the line is not a valid
    /// message.
    Json(serde_json::Error),
    /// A line exceeds [`MAX_LINE_LEN`].
    TooLong { len: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Json(err) => write!(f, "invalid message: {err}"),
            FrameError::TooLong { len } => {
                write!(f, "line of {len} bytes exceeds the limit of {MAX_LINE_LEN}")
            }
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Json(err) => Some(err),
            FrameError::TooLong { .. } => None,
        }
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(err: serde_json::Error) -> Self {
        FrameError::Json(err)
    }
}

/// Encodes one message as a `\n`-terminated line.
pub fn encode_line<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let mut line = serde_json::to_vec(message)?;
    if line.len() > MAX_LINE_LEN {
        return Err(FrameError::TooLong { len: line.len() });
    }
    line.push(b'\n');
    Ok(line)
}

/// Decodes one line, with or without its trailing `\n` / `\r\n`.
pub fn decode_line<T: DeserializeOwned>(line: &[u8]) -> Result<T, FrameError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_LINE_LEN {
        return Err(FrameError::TooLong { len: line.len() });
    }
    Ok(serde_json::from_slice(line)?)
}

/// Splits a byte stream into lines. Feed it whatever the socket returned
/// with [`LineBuffer::push`], then drain complete lines with
/// [`LineBuffer::next_line`].
#[derive(Debug, Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
    /// How far `buf` has been searched for a newline already.
    scanned: usize,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete line without its newline, `Ok(None)` if no
    /// complete line is buffered yet, or an error once the pending line is
    /// longer than [`MAX_LINE_LEN`] (the connection should then be dropped).
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        match self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let end = self.scanned + pos;
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                self.scanned = 0;
                line.pop();
                if line.len() > MAX_LINE_LEN {
                    return Err(FrameError::TooLong { len: line.len() });
                }
                Ok(Some(line))
            }
            None => {
                self.scanned = self.buf.len();
                if self.buf.len() > MAX_LINE_LEN {
                    return Err(FrameError::TooLong {
                        len: self.buf.len(),
                    });
                }
                Ok(None)
            }
        }
    }

    /// Bytes buffered after the last complete line.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}
