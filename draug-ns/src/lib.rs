//! draug-ns: the namespace backend — user/mount/pid/net namespaces,
//! overlayfs root, cgroup v2 limits.
//!
//! Currently a stub: every operation returns `Error::Unsupported` until the
//! namespace plumbing lands. The type exists now so the CLI and MCP server
//! can be wired against `dyn Backend` from day one.

use async_trait::async_trait;
use draug_core::{
    Backend, Error, ExecHandle, ExecRequest, Result, Sandbox, SandboxId, SandboxSpec, SnapshotId,
};

const STUB: &str = "draug-ns backend is not implemented yet";

/// Namespace-based sandbox backend (stub).
#[derive(Debug, Clone, Default)]
pub struct NsBackend;

impl NsBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Backend for NsBackend {
    async fn spawn(&self, _spec: &SandboxSpec) -> Result<Sandbox> {
        Err(Error::Unsupported(STUB.into()))
    }

    async fn exec(&self, _id: &SandboxId, _req: ExecRequest) -> Result<ExecHandle> {
        Err(Error::Unsupported(STUB.into()))
    }

    async fn snapshot(&self, _id: &SandboxId, _name: &str) -> Result<SnapshotId> {
        Err(Error::Unsupported(STUB.into()))
    }

    async fn restore(&self, _id: &SandboxId, _snapshot: &SnapshotId) -> Result<()> {
        Err(Error::Unsupported(STUB.into()))
    }

    async fn destroy(&self, _id: &SandboxId) -> Result<()> {
        Err(Error::Unsupported(STUB.into()))
    }
}
