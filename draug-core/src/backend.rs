//! The `Backend` trait: the seam between draug's frontends (CLI, MCP server)
//! and a concrete isolation mechanism (namespaces today, microVMs later).
//!
//! Async semantics (see DESIGN.md): all methods run on tokio and must not
//! block the executor — implementations push mounts, clones, cgroup writes,
//! and SQLite calls onto `spawn_blocking`. Implementations are cheap,
//! cloneable handles; per-sandbox mutation is serialized internally.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::types::{Sandbox, SandboxId, SandboxSpec, SnapshotId};

/// A request to run one command inside a sandbox.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// argv[0] is the program; no shell is implied.
    pub argv: Vec<String>,
    /// Extra environment on top of the sandbox spec's `env`.
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    /// Guest-enforced deadline in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// One event in an exec's output stream.
#[derive(Debug, Clone)]
pub enum ExecEvent {
    Started { pid: u32 },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// Terminal event. A nonzero code is normal completion, not an error.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Terminal event: the exec itself failed (spawn error, timeout,
    /// protocol breakdown). Kinds match `draug_proto::error_kind`.
    Failed { kind: String, message: String },
}

/// Handle to an in-flight exec. Dropping it cancels the exec (best-effort
/// SIGKILL of the remote process group via the guest).
pub struct ExecHandle {
    events: mpsc::Receiver<ExecEvent>,
}

impl ExecHandle {
    pub fn new(events: mpsc::Receiver<ExecEvent>) -> Self {
        Self { events }
    }

    /// Next event, in the order the guest observed them. `None` after the
    /// terminal `Exited` event (or if the sender side was dropped).
    pub async fn next_event(&mut self) -> Option<ExecEvent> {
        self.events.recv().await
    }
}

/// A sandbox backend. See DESIGN.md for the full contract, including
/// cancellation safety (registry-first intent recording) and idempotency
/// requirements.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Create and start a sandbox from a spec.
    async fn spawn(&self, spec: &SandboxSpec) -> Result<Sandbox>;

    /// Run a command inside a running sandbox, streaming output.
    async fn exec(&self, id: &SandboxId, req: ExecRequest) -> Result<ExecHandle>;

    /// Capture the sandbox's filesystem state (overlay upper layer only —
    /// files, not processes).
    async fn snapshot(&self, id: &SandboxId, name: &str) -> Result<SnapshotId>;

    /// Materialize a NEW sandbox whose writable layer starts from
    /// `snapshot`'s captured layer, on top of the same base image. The
    /// snapshot itself is immutable and can be restored any number of
    /// times; the sandbox it was taken from is left untouched. Files come
    /// back exactly as captured — processes do not (see DESIGN.md).
    async fn restore(&self, snapshot: &SnapshotId, name: Option<String>) -> Result<Sandbox>;

    /// Kill everything, unmount, delete on-disk state, deregister.
    /// Idempotent.
    async fn destroy(&self, id: &SandboxId) -> Result<()>;
}
