//! uid/gid mapping for the sandbox user namespace.
//!
//! Rootless: sandbox uid 0 maps to the invoking user (so upperdir files stay
//! theirs) and sandbox uids 1.. map to the user's subordinate range via the
//! setuid `newuidmap`/`newgidmap` helpers. Running as real root we write the
//! map files directly and identity-map everything.

use std::path::Path;

use draug_core::{Error, Result};
use nix::unistd::{getgid, getuid, Uid, User};

/// Write uid/gid maps for the freshly-unshared process `pid`.
pub fn write_maps(pid: u32) -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        return write_maps_as_root(pid);
    }

    let uid = getuid().as_raw();
    let gid = getgid().as_raw();
    let user = User::from_uid(Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string());

    let (sub_uid_start, sub_uid_count) = lookup_subid("/etc/subuid", &user, uid)?;
    let (sub_gid_start, sub_gid_count) = lookup_subid("/etc/subgid", &user, gid)?;

    run_idmap(
        "newuidmap",
        pid,
        uid,
        sub_uid_start,
        sub_uid_count,
        &user,
        "/etc/subuid",
    )?;
    run_idmap(
        "newgidmap",
        pid,
        gid,
        sub_gid_start,
        sub_gid_count,
        &user,
        "/etc/subgid",
    )?;
    Ok(())
}

fn write_maps_as_root(pid: u32) -> Result<()> {
    let write = |name: &str, content: &str| -> Result<()> {
        std::fs::write(format!("/proc/{pid}/{name}"), content)
            .map_err(|e| Error::io("write id map", e))
    };
    // Full identity map: as real root we don't need subordinate ranges.
    write("uid_map", "0 0 4294967295\n")?;
    write("gid_map", "0 0 4294967295\n")?;
    Ok(())
}

fn run_idmap(
    tool: &str,
    pid: u32,
    own_id: u32,
    sub_start: u32,
    sub_count: u32,
    user: &str,
    subid_file: &str,
) -> Result<()> {
    // Map: sandbox 0 -> caller's id, sandbox 1..count -> subordinate range.
    let out = std::process::Command::new(tool)
        .args([
            pid.to_string(),
            "0".into(),
            own_id.to_string(),
            "1".into(),
            "1".into(),
            sub_start.to_string(),
            sub_count.to_string(),
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Unsupported(format!(
                    "{tool} not found; install it (package `uidmap` on Debian/Ubuntu, \
                     `shadow-utils` on Fedora) to run rootless sandboxes"
                ))
            } else {
                Error::io("run idmap helper", e)
            }
        })?;
    if !out.status.success() {
        return Err(Error::Unsupported(format!(
            "{tool} failed ({}): {}. Check that {subid_file} grants \"{user}\" the range \
             {sub_start}:{sub_count} and that {tool} is setuid root; see subuid(5)",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(())
}

/// Find the subordinate id range for `user` (matching by name or numeric id).
fn lookup_subid(file: &str, user: &str, numeric_id: u32) -> Result<(u32, u32)> {
    let content = std::fs::read_to_string(file).map_err(|e| {
        Error::Unsupported(format!(
            "cannot read {file} ({e}); rootless sandboxes need a subordinate id range. \
             Fix: add the line \"{user}:100000:65536\" to {file} (see subuid(5))"
        ))
    })?;
    match parse_subid(&content, user, numeric_id) {
        Some(range) => Ok(range),
        None => Err(Error::Unsupported(format!(
            "no subordinate id range for \"{user}\" in {file}. \
             Fix: add the line \"{user}:100000:65536\" to {file} (see subuid(5)), \
             or run `usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}`"
        ))),
    }
}

fn parse_subid(content: &str, user: &str, numeric_id: u32) -> Option<(u32, u32)> {
    let numeric = numeric_id.to_string();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ':');
        let (Some(name), Some(start), Some(count)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if name != user && name != numeric {
            continue;
        }
        let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.trim().parse::<u32>()) else {
            continue;
        };
        if count == 0 {
            continue;
        }
        // Sandbox ids 1.. map onto the range; 65535 inner ids is plenty.
        return Some((start, count.min(65535)));
    }
    None
}

/// True if `path` exists (used to pre-check the helpers in diagnostics).
#[allow(dead_code)]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[cfg(test)]
mod tests {
    use super::parse_subid;

    #[test]
    fn parses_by_name_and_uid() {
        let content = "# comment\n\nalice:100000:65536\n1001:200000:1000\n";
        assert_eq!(parse_subid(content, "alice", 1000), Some((100000, 65535)));
        assert_eq!(parse_subid(content, "bob", 1001), Some((200000, 1000)));
        assert_eq!(parse_subid(content, "bob", 1002), None);
    }

    #[test]
    fn skips_malformed_lines() {
        let content = "broken\nalice:xyz:10\nalice:5000:0\nalice:5000:10\n";
        assert_eq!(parse_subid(content, "alice", 1), Some((5000, 10)));
    }
}
