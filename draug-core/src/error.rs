//! Error taxonomy shared across all draug crates.
//!
//! Variants are categorized by what the caller should do about the failure
//! (retry, fix input, destroy the sandbox, ...) — see DESIGN.md.

use crate::types::SandboxState;

pub type Result<T> = std::result::Result<T, Error>;

/// What kind of resource an id refers to, for `NotFound`/`AlreadyExists`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Sandbox,
    Snapshot,
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceKind::Sandbox => write!(f, "sandbox"),
            ResourceKind::Snapshot => write!(f, "snapshot"),
        }
    }
}

/// Exec-level failure classes reported by the guest agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecErrorKind {
    /// The guest could not spawn the requested program (ENOENT, EACCES, ...).
    SpawnFailed,
    /// `timeout_ms` elapsed; the guest killed the process group.
    Timeout,
    /// The exec was cancelled host-side (handle dropped / socket closed).
    Cancelled,
}

impl std::fmt::Display for ExecErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecErrorKind::SpawnFailed => write!(f, "spawn-failed"),
            ExecErrorKind::Timeout => write!(f, "timeout"),
            ExecErrorKind::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Sandbox or snapshot id doesn't exist. Caller bug or stale handle.
    #[error("{kind} not found: {id}")]
    NotFound { kind: ResourceKind, id: String },

    /// Name/id collision on create.
    #[error("{kind} already exists: {id}")]
    AlreadyExists { kind: ResourceKind, id: String },

    /// Spec rejected before touching the kernel. Fix the input.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// Operation not valid for the sandbox's current state.
    #[error("cannot {op} sandbox {id} in state {state:?}")]
    WrongState {
        id: String,
        state: SandboxState,
        op: &'static str,
    },

    /// Host lacks a required kernel facility. Not retryable.
    #[error("unsupported on this host: {0}")]
    Unsupported(String),

    /// Guest agent protocol violation. Sandbox may be poisoned;
    /// recommended handling is destroy + respawn.
    #[error("guest protocol error: {0}")]
    Protocol(String),

    /// The protocol worked but the exec itself failed (spawn error,
    /// timeout, cancellation). A nonzero exit code is NOT an error.
    #[error("exec failed ({kind}): {message}")]
    Exec {
        kind: ExecErrorKind,
        message: String,
    },

    /// Registry (SQLite) failure.
    #[error("registry error: {0}")]
    Registry(#[from] rusqlite::Error),

    /// OS-level failure, labeled with the operation that hit it.
    #[error("io error during {op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Convenience for wrapping an `io::Error` with an operation label.
    pub fn io(op: &'static str, source: std::io::Error) -> Self {
        Error::Io { op, source }
    }
}
