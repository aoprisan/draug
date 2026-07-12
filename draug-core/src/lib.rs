//! draug-core: the `Backend` trait, sandbox/snapshot registry, resource-limit
//! types, exec protocol types, and the shared error taxonomy.
//!
//! See DESIGN.md at the workspace root for the contracts these types encode.

pub mod backend;
pub mod error;
pub mod limits;
pub mod proto;
pub mod registry;
pub mod types;

pub use backend::{Backend, ExecEvent, ExecHandle, ExecRequest};
pub use error::{Error, ExecErrorKind, ResourceKind, Result};
pub use limits::ResourceLimits;
pub use registry::Registry;
pub use types::{
    DiffEntry, DiffKind, Sandbox, SandboxId, SandboxSpec, SandboxState, SnapshotId, SnapshotMeta,
};
