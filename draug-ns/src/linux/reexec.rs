//! Re-exec dispatch: the namespace backend re-executes the host binary
//! (`/proc/self/exe`) to get a clean single-threaded process for namespace
//! setup (unshare(CLONE_NEWUSER) fails in multithreaded processes, and a
//! tokio runtime must never fork).
//!
//! Every binary embedding draug-ns must call [`maybe_run`] first thing in
//! `main`, before any runtime or threads exist.

use nix::sched::{unshare, CloneFlags};

use super::setup;

pub const REEXEC_ENV: &str = "DRAUG_REEXEC";
pub const CONFIG_ENV: &str = "DRAUG_SETUP_CONFIG";
pub const CLEANUP_DIR_ENV: &str = "DRAUG_CLEANUP_DIR";
pub const COPY_SRC_ENV: &str = "DRAUG_COPY_SRC";
pub const COPY_DST_ENV: &str = "DRAUG_COPY_DST";

pub const MODE_SETUP: &str = "setup";
pub const MODE_PROBE: &str = "probe";
pub const MODE_CLEANUP: &str = "cleanup";
pub const MODE_COPY: &str = "copy";

/// If this process was re-executed for a draug-ns helper role, run that role
/// and never return. Otherwise, return immediately.
pub fn maybe_run() {
    let mode = match std::env::var(REEXEC_ENV) {
        Ok(m) => m,
        Err(_) => return,
    };
    match mode.as_str() {
        MODE_SETUP => {
            let cfg = match std::env::var(CONFIG_ENV)
                .map_err(|e| e.to_string())
                .and_then(|j| serde_json::from_str(&j).map_err(|e| e.to_string()))
            {
                Ok(cfg) => cfg,
                Err(e) => {
                    println!("err bad setup config: {e}");
                    std::process::exit(1);
                }
            };
            setup::run(cfg) // never returns
        }
        MODE_PROBE => match unshare(CloneFlags::CLONE_NEWUSER) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!(
                    "unshare(CLONE_NEWUSER) failed: {e}. Unprivileged user namespaces \
                     appear to be disabled (check `sysctl kernel.unprivileged_userns_clone` \
                     on Debian-family kernels, `sysctl user.max_user_namespaces`, and any \
                     seccomp/AppArmor policy of the containing environment)."
                );
                std::process::exit(3);
            }
        },
        MODE_CLEANUP => {
            let dir = std::env::var(CLEANUP_DIR_ENV).unwrap_or_default();
            cleanup(&dir) // never returns
        }
        MODE_COPY => copy(), // never returns
        other => {
            eprintln!("draug-ns: unknown re-exec mode {other:?}");
            std::process::exit(2);
        }
    }
}

/// Delete a state directory that contains files owned by subordinate uids:
/// enter a user namespace with the same mapping used at spawn time so the
/// files map back to us, then remove the tree. Speaks the same line protocol
/// as setup ("unshared" -> host writes maps -> "go").
fn cleanup(dir: &str) -> ! {
    use std::io::BufRead;
    let fail = |msg: String| -> ! {
        println!("err {}", msg.replace('\n', " "));
        std::process::exit(1);
    };
    if dir.is_empty() {
        fail("cleanup: no directory given".into());
    }
    if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
        fail(format!("cleanup: unshare(CLONE_NEWUSER): {e}"));
    }
    println!("unshared");
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() || line.trim() != "go" {
        fail("cleanup: host did not confirm uid mapping".into());
    }
    if let Err(e) = std::fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            fail(format!("cleanup: remove {dir}: {e}"));
        }
    }
    println!("done");
    std::process::exit(0);
}

/// Copy a layer between two host directories that may contain files owned
/// by subordinate uids and overlayfs whiteouts: enter a user namespace with
/// the spawn-time mapping so those files map back to ids we control, then
/// run the ordinary tree copy. Same line protocol as setup/cleanup; ends
/// with `done <bytes-copied>`.
fn copy() -> ! {
    use std::io::BufRead;
    let fail = |msg: String| -> ! {
        println!("err {}", msg.replace('\n', " "));
        std::process::exit(1);
    };
    let src = std::env::var_os(COPY_SRC_ENV).unwrap_or_default();
    let dst = std::env::var_os(COPY_DST_ENV).unwrap_or_default();
    if src.is_empty() || dst.is_empty() {
        fail("copy: missing source or destination".into());
    }
    if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
        fail(format!("copy: unshare(CLONE_NEWUSER): {e}"));
    }
    println!("unshared");
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() || line.trim() != "go" {
        fail("copy: host did not confirm uid mapping".into());
    }
    match super::fscopy::copy_tree(std::path::Path::new(&src), std::path::Path::new(&dst)) {
        Ok(bytes) => {
            println!("done {bytes}");
            std::process::exit(0);
        }
        Err(e) => fail(format!("copy: {e}")),
    }
}
