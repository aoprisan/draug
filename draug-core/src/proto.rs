//! Exec protocol types: length-prefixed msgpack over a unix socket.
//!
//! Framing: `u32` big-endian length, then that many bytes of one
//! msgpack-encoded message. One connection == one exec. Messages are
//! encoded as `(discriminant, payload)` tuples via serde's default
//! externally-indexed enum representation in rmp-serde.
//!
//! This module deliberately has no tokio dependency in its types so the
//! static guest binary can eventually link it without the async runtime.

use serde::{Deserialize, Serialize};

/// Maximum size of a single frame. The guest rejects larger frames with a
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
    Stdin { data: Vec<u8> },
}

/// Messages sent guest → host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestMessage {
    /// The process was spawned.
    Started { pid: u32 },
    /// A chunk of stdout, at most `MAX_CHUNK_LEN` bytes.
    Stdout { data: Vec<u8> },
    /// A chunk of stderr, at most `MAX_CHUNK_LEN` bytes.
    Stderr { data: Vec<u8> },
    /// Terminal frame: normal completion. Exactly one of `code`/`signal`.
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Terminal frame: the exec itself failed (spawn error, timeout, ...).
    Error { kind: String, message: String },
}
