//! draug-proto: wire types + synchronous framing for the exec protocol.
//!
//! Transport: one unix stream connection per exec. Framing: `u32` big-endian
//! length, then that many bytes of one msgpack-encoded message (rmp-serde).
//! This crate is deliberately tokio-free so the static guest agent can link
//! it without the async runtime; async framing lives host-side in draug-ns.

use serde::{Deserialize, Serialize};

/// Maximum size of a single frame. Both ends reject larger frames as a
/// protocol error.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Stdout/stderr chunks are cut to at most this many bytes so one stream
/// cannot starve the other behind a giant frame.
pub const MAX_CHUNK_LEN: usize = 64 * 1024;

/// Messages sent host → guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    /// Exactly one, first frame on the connection.
    ExecRequest {
        /// argv[0] is the program; no shell is implied.
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        /// Whether `Stdin` frames will follow.
        stdin: bool,
        /// Allocate a pty instead of pipes (not yet implemented).
        tty: bool,
        /// Guest-enforced deadline; SIGKILL + `Error{kind:"timeout"}` on expiry.
        timeout_ms: Option<u64>,
    },
    /// Zero or more, only if `stdin: true`. Empty `data` means EOF.
    Stdin {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
}

/// Messages sent guest → host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestMessage {
    /// The process was spawned.
    Started { pid: u32 },
    /// A chunk of stdout, at most `MAX_CHUNK_LEN` bytes.
    Stdout {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// A chunk of stderr, at most `MAX_CHUNK_LEN` bytes.
    Stderr {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Terminal frame: normal completion. Exactly one of `code`/`signal`.
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Terminal frame: the exec itself failed (spawn error, timeout, ...).
    Error { kind: String, message: String },
}

/// Well-known `GuestMessage::Error` kinds.
pub mod error_kind {
    pub const SPAWN_FAILED: &str = "spawn-failed";
    pub const TIMEOUT: &str = "timeout";
    pub const PROTOCOL: &str = "protocol";
}

/// Blocking frame IO over any `Read`/`Write` (used by the guest; the host
/// uses async equivalents in draug-ns).
pub mod sync_io {
    use std::io::{self, Read, Write};

    use serde::de::DeserializeOwned;
    use serde::Serialize;

    use super::MAX_FRAME_LEN;

    /// Read one frame. Returns `Ok(None)` on clean EOF at a frame boundary.
    pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len_buf);
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame of {len} bytes exceeds MAX_FRAME_LEN"),
            ));
        }
        let mut buf = vec![0u8; len as usize];
        r.read_exact(&mut buf)?;
        rmp_serde::from_slice(&buf)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad frame: {e}")))
    }

    /// Write one frame and flush.
    pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
        let body = rmp_serde::to_vec(msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("encode: {e}")))?;
        let len = u32::try_from(body.len())
            .ok()
            .filter(|l| *l <= MAX_FRAME_LEN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
        w.write_all(&len.to_be_bytes())?;
        w.write_all(&body)?;
        w.flush()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{GuestMessage, HostMessage};

        #[test]
        fn roundtrip() {
            let mut buf = Vec::new();
            let msg = HostMessage::ExecRequest {
                argv: vec!["echo".into(), "hi".into()],
                env: vec![("A".into(), "b".into())],
                cwd: None,
                stdin: false,
                tty: false,
                timeout_ms: Some(1000),
            };
            write_frame(&mut buf, &msg).unwrap();
            write_frame(&mut buf, &GuestMessage::Stdout { data: b"hi\n".to_vec() }).unwrap();

            let mut r = buf.as_slice();
            let got: HostMessage = read_frame(&mut r).unwrap().unwrap();
            assert!(matches!(got, HostMessage::ExecRequest { ref argv, .. } if argv[0] == "echo"));
            let got: GuestMessage = read_frame(&mut r).unwrap().unwrap();
            assert!(matches!(got, GuestMessage::Stdout { ref data } if data == b"hi\n"));
            assert!(read_frame::<GuestMessage>(&mut r).unwrap().is_none());
        }
    }
}
