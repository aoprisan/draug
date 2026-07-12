//! draug-ns: rootless Linux sandbox backend.
//!
//! Isolation: user + mount + pid + net + uts namespaces, an overlayfs root
//! over the user's project directory, and (where delegated) cgroup v2
//! resource limits. Zero-daemon: each sandbox is kept alive by its own
//! PID 1 (the guest agent, forked from a short-lived setup process); host
//! invocations find it again via the registry + a pidfile.
//!
//! Process dance for `spawn` (rootless):
//!
//! ```text
//! host (sbx) ── re-exec /proc/self/exe (DRAUG_REEXEC=setup) ──> setup
//!   setup: unshare(NEWUSER); print "unshared"; wait
//!   host:  newuidmap/newgidmap (or direct map when root); cgroup add; "go"
//!   setup: unshare(NEWNS|NEWPID|NEWNET|NEWUTS); mounts; pivot_root; fork
//!            child  = guest agent (PID 1 in sandbox): binds socket, "ready"
//!            parent = prints "pid <host-pid>", exits
//! ```
//!
//! Anything that embeds this backend must call [`reexec::maybe_run`] at the
//! very top of `main`, before starting a tokio runtime — the setup process
//! is a re-execution of the host binary itself.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{backend::NsBackend, userns_available};

#[cfg(target_os = "linux")]
pub mod reexec {
    pub use crate::linux::reexec::maybe_run;
}

#[cfg(not(target_os = "linux"))]
mod other {
    use async_trait::async_trait;
    use draug_core::{
        Backend, Error, ExecHandle, ExecRequest, Registry, Result, Sandbox, SandboxId,
        SandboxSpec, SnapshotId,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    const MSG: &str = "the draug-ns backend requires Linux";

    /// Stub so the workspace (and its tests) compile on non-Linux hosts.
    #[derive(Clone)]
    pub struct NsBackend;

    impl NsBackend {
        pub fn new(_registry: Arc<Registry>, _state_root: PathBuf) -> Self {
            Self
        }

        pub async fn diff(&self, _target: &str) -> Result<Vec<draug_core::DiffEntry>> {
            Err(Error::Unsupported(MSG.into()))
        }

        pub async fn reconcile(&self) -> Result<usize> {
            Ok(0)
        }
    }

    #[async_trait]
    impl Backend for NsBackend {
        async fn spawn(&self, _spec: &SandboxSpec) -> Result<Sandbox> {
            Err(Error::Unsupported(MSG.into()))
        }
        async fn exec(&self, _id: &SandboxId, _req: ExecRequest) -> Result<ExecHandle> {
            Err(Error::Unsupported(MSG.into()))
        }
        async fn snapshot(&self, _id: &SandboxId, _name: &str) -> Result<SnapshotId> {
            Err(Error::Unsupported(MSG.into()))
        }
        async fn restore(&self, _snapshot: &SnapshotId, _name: Option<String>) -> Result<Sandbox> {
            Err(Error::Unsupported(MSG.into()))
        }
        async fn destroy(&self, _id: &SandboxId) -> Result<()> {
            Err(Error::Unsupported(MSG.into()))
        }
    }

    pub fn userns_available() -> std::result::Result<(), String> {
        Err(MSG.into())
    }

    pub mod reexec {
        /// No-op off Linux.
        pub fn maybe_run() {}
    }
}

#[cfg(not(target_os = "linux"))]
pub use other::{reexec, userns_available, NsBackend};
