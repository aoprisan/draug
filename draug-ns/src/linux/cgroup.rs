//! cgroup v2 resource limits.
//!
//! Only attempted when the spec actually sets limits; a host without a
//! delegated cgroup v2 subtree gets an actionable `Unsupported` error rather
//! than silently unenforced limits.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use draug_core::{Error, ResourceLimits, Result};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub struct CgroupHandle {
    pub path: PathBuf,
}

/// Create a cgroup for sandbox `id` and write the requested limits.
/// Returns `Ok(None)` when no limits were requested.
pub fn create(id: &str, limits: &ResourceLimits) -> Result<Option<CgroupHandle>> {
    let mut controllers: Vec<&str> = Vec::new();
    if limits.memory_bytes.is_some() {
        controllers.push("memory");
    }
    if limits.cpu_millis.is_some() {
        controllers.push("cpu");
    }
    if limits.pids.is_some() {
        controllers.push("pids");
    }
    if controllers.is_empty() {
        return Ok(None);
    }

    let base = own_cgroup_dir()?;
    let available = std::fs::read_to_string(base.join("cgroup.controllers"))
        .map_err(|e| Error::io("read cgroup.controllers", e))?;
    for c in &controllers {
        if !available.split_whitespace().any(|a| a == *c) {
            return Err(Error::Unsupported(format!(
                "cgroup controller \"{c}\" is not available in {} (available: {}). \
                 Resource limits need a delegated cgroup v2 subtree: on systemd hosts set \
                 Delegate=cpu memory pids for user@.service, or launch via \
                 `systemd-run --user --scope -p Delegate=yes ...`, or omit the limits",
                base.display(),
                available.trim()
            )));
        }
    }

    enable_controllers(&base, &controllers)?;

    let dir = base.join(format!("draug-{id}"));
    std::fs::create_dir(&dir).map_err(|e| Error::io("create sandbox cgroup", e))?;
    let write_limit = |file: &str, value: String| -> Result<()> {
        std::fs::write(dir.join(file), value).map_err(|e| Error::io("write cgroup limit", e))
    };
    if let Some(bytes) = limits.memory_bytes {
        write_limit("memory.max", bytes.to_string())?;
    }
    if let Some(pids) = limits.pids {
        write_limit("pids.max", pids.to_string())?;
    }
    if let Some(millis) = limits.cpu_millis {
        // period 100ms; 1000 millicores == one full core == quota 100000.
        write_limit("cpu.max", format!("{} 100000", u64::from(millis) * 100))?;
    }
    Ok(Some(CgroupHandle { path: dir }))
}

/// Move `pid` into the sandbox cgroup.
pub fn add_pid(handle: &CgroupHandle, pid: u32) -> Result<()> {
    std::fs::write(handle.path.join("cgroup.procs"), pid.to_string())
        .map_err(|e| Error::io("move process into cgroup", e))
}

/// Best-effort teardown: kill any stragglers, then remove the directory.
pub fn destroy(path: &Path) {
    let _ = std::fs::write(path.join("cgroup.kill"), "1");
    for _ in 0..20 {
        match std::fs::remove_dir(path) {
            Ok(()) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// The cgroup v2 directory this process lives in.
fn own_cgroup_dir() -> Result<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| Error::io("read /proc/self/cgroup", e))?;
    let rel = parse_own_cgroup(&content).ok_or_else(|| {
        Error::Unsupported(
            "no cgroup v2 hierarchy found in /proc/self/cgroup; resource limits require \
             the unified cgroup v2 hierarchy mounted at /sys/fs/cgroup"
                .into(),
        )
    })?;
    let dir = PathBuf::from(CGROUP_ROOT).join(rel.trim_start_matches('/'));
    if !dir.join("cgroup.controllers").exists() {
        return Err(Error::Unsupported(format!(
            "{} does not look like a cgroup v2 directory; this host appears to use the \
             legacy (v1/hybrid) cgroup layout, which draug does not support for limits",
            dir.display()
        )));
    }
    Ok(dir)
}

fn parse_own_cgroup(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim())
}

/// Enable `controllers` in `base`'s subtree_control. If that fails with
/// EBUSY (the no-internal-processes rule: `base` still hosts processes),
/// migrate every process in `base` to a `leaf` child cgroup and retry —
/// only safe because everything in a delegated subtree belongs to us.
fn enable_controllers(base: &Path, controllers: &[&str]) -> Result<()> {
    let payload = controllers
        .iter()
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ");
    let subtree = base.join("cgroup.subtree_control");
    let first = std::fs::write(&subtree, &payload);
    let err = match first {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if err.raw_os_error() != Some(libc::EBUSY) {
        return Err(map_subtree_err(err, base));
    }

    let leaf = base.join("leaf");
    if let Err(e) = std::fs::create_dir(&leaf) {
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(Error::io("create leaf cgroup", e));
        }
    }
    let procs = std::fs::read_to_string(base.join("cgroup.procs"))
        .map_err(|e| Error::io("read cgroup.procs", e))?;
    for pid in procs.split_whitespace() {
        // ESRCH races (a process exiting mid-migration) are fine.
        let _ = std::fs::write(leaf.join("cgroup.procs"), pid);
    }
    std::fs::write(&subtree, &payload).map_err(|e| map_subtree_err(e, base))
}

fn map_subtree_err(e: std::io::Error, base: &Path) -> Error {
    if e.kind() == ErrorKind::PermissionDenied {
        Error::Unsupported(format!(
            "not allowed to manage cgroups under {} — the subtree is not delegated to \
             this user. On systemd hosts enable delegation (Delegate=cpu memory pids in \
             user@.service drop-in) or launch via `systemd-run --user --scope -p \
             Delegate=yes ...`, or omit the resource limits",
            base.display()
        ))
    } else {
        Error::io("enable cgroup controllers", e)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_own_cgroup;

    #[test]
    fn finds_v2_line_in_hybrid_output() {
        let content = "7:pids:/x\n1:cpu:/\n0::/user.slice/user-1000.slice/session.scope\n";
        assert_eq!(
            parse_own_cgroup(content),
            Some("/user.slice/user-1000.slice/session.scope")
        );
        assert_eq!(parse_own_cgroup("1:cpu:/\n"), None);
    }
}
