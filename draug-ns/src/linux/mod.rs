pub mod backend;
pub mod cgroup;
pub mod reexec;
pub mod setup;
pub mod uidmap;

/// Check whether unprivileged user namespaces work on this host, by
/// re-executing ourselves and attempting `unshare(CLONE_NEWUSER)` in the
/// child (a probe in-process would poison this process's namespaces).
pub fn userns_available() -> Result<(), String> {
    let out = std::process::Command::new("/proc/self/exe")
        .env(reexec::REEXEC_ENV, reexec::MODE_PROBE)
        .output()
        .map_err(|e| format!("failed to re-exec self for probe: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
