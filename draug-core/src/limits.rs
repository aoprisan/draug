//! Resource limit types. Enforced by backends via cgroup v2 (cpu.max,
//! memory.max, pids.max) plus a disk quota on the overlay upper layer.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Resource limits for a sandbox. All optional; `None` means unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU budget in millicores; 1000 = one full core (cgroup `cpu.max`).
    pub cpu_millis: Option<u32>,
    /// Memory ceiling in bytes (cgroup `memory.max`).
    pub memory_bytes: Option<u64>,
    /// Maximum number of processes/threads (cgroup `pids.max`).
    pub pids: Option<u32>,
    /// Quota for the sandbox's writable (upper) layer, in bytes.
    pub disk_bytes: Option<u64>,
    /// Whole-sandbox wall-clock deadline, enforced host-side.
    pub wall_time: Option<Duration>,
}

impl ResourceLimits {
    /// No limits at all.
    pub fn unlimited() -> Self {
        Self::default()
    }
}
