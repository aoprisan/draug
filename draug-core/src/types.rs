//! Core identifier and specification types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::limits::ResourceLimits;

/// Opaque sandbox identifier (short random id; also the state dir name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxId(pub String);

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SandboxId {
    fn from(s: &str) -> Self {
        SandboxId(s.to_owned())
    }
}

/// Opaque snapshot identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub String);

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SnapshotId {
    fn from(s: &str) -> Self {
        SnapshotId(s.to_owned())
    }
}

/// Lifecycle state of a sandbox, persisted in the registry.
///
/// `Creating → Ready ⇄ Running → Stopped`; `destroy()` removes the row
/// entirely rather than recording a `Destroyed` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState {
    /// Row exists, kernel state may be partial. GC-able.
    Creating,
    /// Fully set up, no exec in flight.
    Ready,
    /// At least one exec in flight.
    Running,
    /// Processes gone, filesystem retained (restorable / destroyable).
    Stopped,
}

impl SandboxState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxState::Creating => "creating",
            SandboxState::Ready => "ready",
            SandboxState::Running => "running",
            SandboxState::Stopped => "stopped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "creating" => Some(SandboxState::Creating),
            "ready" => Some(SandboxState::Ready),
            "running" => Some(SandboxState::Running),
            "stopped" => Some(SandboxState::Stopped),
            _ => None,
        }
    }
}

/// Everything needed to create a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Optional human-readable name (unique across live sandboxes).
    pub name: Option<String>,
    /// Read-only base image directory (overlayfs lowerdir).
    pub rootfs: PathBuf,
    /// Seed the writable layer from an existing snapshot instead of empty.
    pub from_snapshot: Option<SnapshotId>,
    /// Resource limits; `ResourceLimits::unlimited()` for none.
    pub limits: ResourceLimits,
    /// Environment variables set for every exec in this sandbox.
    pub env: Vec<(String, String)>,
    /// Whether the sandbox gets (loopback-only) network access.
    pub network: bool,
    /// Insecure escape hatch: if a private `/proc` cannot be mounted (a host
    /// that masks parts of `/proc`, e.g. running draug inside a hardened
    /// container), fall back to bind-mounting the *host's* `/proc`. This
    /// exposes host processes via `/proc/<pid>/{root,cwd,fd}` — a filesystem
    /// escape to the invoking user's own files — so it is **off by default**;
    /// spawn fails closed instead. Only enable on a trusted host.
    pub allow_host_proc_fallback: bool,
}

/// A live sandbox as recorded in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: SandboxId,
    pub name: Option<String>,
    pub state: SandboxState,
    /// Which backend owns this sandbox (e.g. "ns").
    pub backend: String,
    /// Base image path (overlayfs lowerdir).
    pub rootfs: PathBuf,
    /// Directory holding upper/work/merged and the guest socket.
    pub state_dir: PathBuf,
    pub limits: ResourceLimits,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

/// Snapshot metadata as recorded in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub id: SnapshotId,
    /// Sandbox this was taken from; `None` if that sandbox was destroyed.
    pub sandbox_id: Option<SandboxId>,
    pub name: String,
    /// Directory holding the captured upper layer (in `upper/` beneath it).
    pub path: PathBuf,
    /// Base image the captured layer applies on top of (overlayfs lowerdir
    /// of the sandbox the snapshot was taken from).
    pub rootfs: PathBuf,
    /// Total bytes of regular-file content in the captured layer.
    pub size_bytes: u64,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

/// How a path in an overlay upper layer differs from the base image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// Present in the layer, absent from the base.
    Added,
    /// Present in both; the layer's version shadows the base's.
    Modified,
    /// Present in the base, removed by the layer (overlayfs whiteout or
    /// a path shadowed away by an opaque/replacing entry).
    Deleted,
}

/// One changed path in a layer-vs-base diff.
///
/// `size`/`mode` describe the layer's version for `Added`/`Modified` and the
/// base's (now gone) version for `Deleted`; they are `None` where they carry
/// no information (e.g. directory sizes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    /// Path relative to the base image root, `/`-separated.
    pub path: String,
    pub kind: DiffKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Full `st_mode` in octal (e.g. `"100644"`, `"40755"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}
