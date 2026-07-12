//! draug-guest: PID 1 inside a draug sandbox.
//!
//! Listens on a unix socket and speaks the length-prefixed msgpack exec
//! protocol (see DESIGN.md): one connection == one exec, streaming
//! stdout/stderr, guest-enforced timeouts, connection close == cancel.
//!
//! Entered either by the `draug-guest` binary (future static-musl guest for
//! VM backends) or, in the namespace backend, by `fork()` from the setup
//! process — so `guest_main` must not assume it was exec'd.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Everything the guest needs to run. Constructed by draug-ns's setup
/// process (paths are *inside-sandbox* paths, post-pivot_root).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestConfig {
    /// Where to bind the exec socket (e.g. /run/draug/guest.sock).
    pub socket_path: PathBuf,
    /// Where to send the guest's own stdout/stderr after startup.
    pub log_path: PathBuf,
    /// Default working directory for execs (the project dir).
    pub default_cwd: PathBuf,
    /// Base environment for exec'd commands (PATH, HOME, ... + sandbox env).
    pub env: Vec<(String, String)>,
    /// Mount a fresh /proc (required when entered as PID 1 of a new pidns).
    pub mount_proc: bool,
}

#[cfg(target_os = "linux")]
mod server;

#[cfg(target_os = "linux")]
pub use server::guest_main;

#[cfg(not(target_os = "linux"))]
pub fn guest_main(_cfg: GuestConfig) -> ! {
    eprintln!("draug-guest: only supported on Linux");
    std::process::exit(1);
}
