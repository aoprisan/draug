//! The sandbox setup process: re-exec'd from the host binary, single
//! threaded, no tokio. Builds the namespaces and filesystem, then forks the
//! guest agent as PID 1 of the new pid namespace.
//!
//! Talks to the host over stdin/stdout with a trivial line protocol:
//! setup prints `unshared`, waits for `go` (host writes the uid/gid maps and
//! cgroup membership in between), later prints `pid <hostpid>`; the guest
//! prints `ready` once its socket is bound. Any failure prints `err <msg>`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use draug_guest::GuestConfig;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::unistd::{chdir, fork, ForkResult};
use serde::{Deserialize, Serialize};

/// Path (inside the sandbox) where the host-visible runtime dir is bound.
pub const INSIDE_RT_DIR: &str = "/run/draug";
/// Socket filename within the runtime dir, both views.
pub const SOCKET_NAME: &str = "guest.sock";
/// Guest log filename within the runtime dir, both views.
pub const LOG_NAME: &str = "guest.log";

/// Host directories bound read-only into the sandbox root. `/home`, `/root`,
/// and anything else user-writable is deliberately absent: the only writable
/// window onto the host is the project overlay's upper layer.
const RO_SYSTEM_DIRS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/var",
    "/nix", "/snap",
];

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupConfig {
    pub id: String,
    pub state_dir: PathBuf,
    pub project_dir: PathBuf,
    pub hostname: String,
    /// Sandbox environment (layered over the guest's base env).
    pub env: Vec<(String, String)>,
    /// Keep the host network namespace instead of an isolated one.
    pub network: bool,
    /// Whether the *host* side runs as real root (identity uid map, no
    /// userxattr needed for overlayfs).
    pub host_is_root: bool,
}

pub fn run(cfg: SetupConfig) -> ! {
    if let Err(msg) = run_inner(&cfg) {
        // Single line; the host parses it into an error.
        println!("err {}", msg.replace('\n', " "));
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    unreachable!("run_inner only returns on error");
}

fn run_inner(cfg: &SetupConfig) -> Result<(), String> {
    // 1. User namespace first; the host writes our uid/gid maps.
    unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| {
        format!(
            "unshare(CLONE_NEWUSER) failed: {e}. Unprivileged user namespaces appear \
             to be unavailable on this host"
        )
    })?;
    println!("unshared");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("waiting for host: {e}"))?;
    if line.trim() != "go" {
        return Err("host aborted during uid mapping".into());
    }

    // 2. Remaining namespaces. We are now (mapped) root with full
    //    capabilities over everything we unshare.
    let mut flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWUTS;
    if !cfg.network {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    unshare(flags).map_err(|e| format!("unshare(mount/pid/uts/net): {e}"))?;

    if !cfg.network {
        bring_up_loopback()?;
    }
    nix::unistd::sethostname(&cfg.hostname).map_err(|e| format!("sethostname: {e}"))?;

    // 3. Filesystem: assemble the new root. The pivot happens in the child:
    //    mounting a fresh /proc for the new pid namespace requires (a) being
    //    *in* that namespace, i.e. after the fork, and (b) an existing,
    //    fully-visible procfs mount in the mount namespace, i.e. before the
    //    old root is detached.
    let root = build_rootfs(cfg)?;

    // 4. PID 1: fork the guest agent into the new pid namespace.
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Parent { child } => {
            println!("pid {child}");
            let _ = std::io::stdout().flush();
            // The guest owns the namespaces now; nothing left to do here.
            std::process::exit(0);
        }
        ForkResult::Child => {
            if let Err(msg) = mount_proc_and_pivot(&root) {
                println!("err {}", msg.replace('\n', " "));
                let _ = std::io::stdout().flush();
                std::process::exit(1);
            }
            let rt = Path::new(INSIDE_RT_DIR);
            draug_guest::guest_main(GuestConfig {
                socket_path: rt.join(SOCKET_NAME),
                log_path: rt.join(LOG_NAME),
                default_cwd: cfg.project_dir.clone(),
                env: base_env(cfg),
                mount_proc: false, // done here, where the old root is still visible
            })
        }
    }
}

/// Runs as PID 1 of the new pid namespace, old root still attached.
fn mount_proc_and_pivot(root: &Path) -> Result<(), String> {
    let proc_dir = root.join("proc");
    let fresh = mount(
        Some("proc"),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    );
    if fresh.is_err() {
        // Hosts that mask parts of /proc (hardened container runtimes) fail
        // the kernel's "fully visible" rule for fresh proc mounts in a user
        // namespace. Falling back to the host's procfs keeps tools working,
        // at the cost of showing host-pidns pids.
        bind(Path::new("/proc"), &proc_dir, MsFlags::MS_REC)
            .map_err(|e| format!("fresh proc mount refused and bind fallback failed ({e})"))?;
        println!(
            "warn cannot mount a private /proc (host /proc is masked?); \
             the sandbox sees the host's /proc instead"
        );
        let _ = std::io::stdout().flush();
    }

    chdir(root).map_err(|e| format!("chdir(new root): {e}"))?;
    nix::unistd::pivot_root(".", ".").map_err(|e| format!("pivot_root: {e}"))?;
    umount2(".", MntFlags::MNT_DETACH).map_err(|e| format!("detach old root: {e}"))?;
    chdir("/").map_err(|e| format!("chdir(/): {e}"))?;
    Ok(())
}

fn base_env(cfg: &SetupConfig) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("HOME".to_string(), "/root".to_string()),
        ("USER".to_string(), "root".to_string()),
        ("LOGNAME".to_string(), "root".to_string()),
        ("TERM".to_string(), "xterm".to_string()),
    ];
    env.extend(cfg.env.iter().cloned());
    env
}

// --- filesystem assembly -----------------------------------------------------

fn build_rootfs(cfg: &SetupConfig) -> Result<PathBuf, String> {
    let root = cfg.state_dir.join("root");

    // Nothing we mount may propagate back to the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_SLAVE,
        None::<&str>,
    )
    .map_err(|e| format!("make / rslave: {e}"))?;

    mount_tmpfs(&root, "mode=755")?;

    for dir in RO_SYSTEM_DIRS {
        bind_system_dir(&root, dir)?;
    }

    setup_dev(&root)?;

    mkdir(&root.join("proc"))?; // the guest mounts proc from inside the pidns
    mkdir(&root.join("root"))?;
    let tmp = root.join("tmp");
    mount_tmpfs(&tmp, "mode=1777")?;

    setup_sys(&root, cfg.network);

    // Host-visible runtime dir (socket + guest log) at /run/draug.
    let run = root.join("run");
    mount_tmpfs(&run, "mode=755")?;
    let run_draug = run.join("draug");
    mkdir(&run_draug)?;
    bind(&cfg.state_dir.join("rt"), &run_draug, MsFlags::empty())?;

    mount_project_overlay(cfg, &root)?;

    Ok(root)
}

fn mount_project_overlay(cfg: &SetupConfig, root: &Path) -> Result<(), String> {
    let target = root.join(
        cfg.project_dir
            .strip_prefix("/")
            .map_err(|_| "project dir must be absolute".to_string())?,
    );
    std::fs::create_dir_all(&target).map_err(|e| format!("mkdir {}: {e}", target.display()))?;

    let upper = cfg.state_dir.join("upper");
    let work = cfg.state_dir.join("work");
    match overlay(cfg, &target, &upper, &work) {
        Ok(()) => Ok(()),
        // EINVAL/ENOTSUP typically means the upperdir's filesystem can't
        // host an overlay upper layer (e.g. the state dir itself lives on
        // overlayfs, as in many CI containers). Fall back to a tmpfs-backed
        // upper inside the sandbox's mount namespace.
        Err(nix::errno::Errno::EINVAL) | Err(nix::errno::Errno::ENOTSUP) => {
            let ovl = cfg.state_dir.join("ovl");
            mount_tmpfs(&ovl, "mode=755")?;
            let (upper, work) = (ovl.join("upper"), ovl.join("work"));
            mkdir(&upper)?;
            mkdir(&work)?;
            overlay(cfg, &target, &upper, &work)
                .map_err(|e| format!("overlay mount (tmpfs fallback): {e}"))?;
            println!(
                "warn state dir cannot host an overlay upper layer (nested overlayfs?); \
                 using a tmpfs upper — changes to the project will NOT survive destroy"
            );
            let _ = std::io::stdout().flush();
            Ok(())
        }
        Err(e) => Err(format!(
            "overlay mount: {e} (lower={}, upper={}); kernel >= 5.11 with unprivileged \
             overlayfs support is required",
            cfg.project_dir.display(),
            upper.display()
        )),
    }
}

fn overlay(
    cfg: &SetupConfig,
    target: &Path,
    upper: &Path,
    work: &Path,
) -> Result<(), nix::errno::Errno> {
    let mut opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        cfg.project_dir.display(),
        upper.display(),
        work.display()
    );
    // Rootless overlay cannot write trusted.* xattrs; userxattr (5.11+)
    // stores overlay metadata in user.* instead.
    if !cfg.host_is_root {
        opts.push_str(",userxattr");
    }
    mount(
        Some("overlay"),
        target,
        Some("overlay"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(opts.as_str()),
    )
}

fn bind_system_dir(root: &Path, dir: &str) -> Result<(), String> {
    let src = Path::new(dir);
    let meta = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(_) => return Ok(()), // absent on this distro
    };
    let target = root.join(&dir[1..]);
    if meta.is_symlink() {
        // e.g. /bin -> usr/bin on merged-usr distros
        let dest = std::fs::read_link(src).map_err(|e| format!("readlink {dir}: {e}"))?;
        std::os::unix::fs::symlink(&dest, &target)
            .map_err(|e| format!("symlink {}: {e}", target.display()))?;
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    mkdir(&target)?;
    bind(src, &target, MsFlags::MS_REC)?;
    remount_ro_recursive(&target)?;
    Ok(())
}

/// Remount `top` and every mount below it read-only, preserving each
/// mount's existing flags (dropping flags of a mount inherited from a more
/// privileged namespace is refused by the kernel).
fn remount_ro_recursive(top: &Path) -> Result<(), String> {
    for (mountpoint, flags) in mounts_under(top)? {
        let remount = MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY | flags;
        if let Err(e) = mount(None::<&str>, &mountpoint, None::<&str>, remount, None::<&str>) {
            // A locked submount we can't touch: tolerable for submounts,
            // fatal for the top-level dir (it would stay writable).
            if mountpoint == top {
                return Err(format!("remount {} read-only: {e}", top.display()));
            }
        }
    }
    Ok(())
}

/// (mountpoint, existing flags) for `top` and everything mounted beneath it,
/// from /proc/self/mountinfo.
fn mounts_under(top: &Path) -> Result<Vec<(PathBuf, MsFlags)>, String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read mountinfo: {e}"))?;
    let mut out = Vec::new();
    for line in info.lines() {
        let mut fields = line.split(' ');
        let (Some(mp), Some(opts)) = (fields.nth(4), fields.next()) else {
            continue;
        };
        let mp = PathBuf::from(unescape_mountinfo(mp));
        if mp == top || mp.starts_with(top) {
            out.push((mp, parse_mount_opts(opts)));
        }
    }
    // Parents before children so the top-level remount happens first.
    out.sort_by_key(|(p, _)| p.components().count());
    Ok(out)
}

fn parse_mount_opts(opts: &str) -> MsFlags {
    let mut flags = MsFlags::empty();
    for o in opts.split(',') {
        flags |= match o {
            "nosuid" => MsFlags::MS_NOSUID,
            "nodev" => MsFlags::MS_NODEV,
            "noexec" => MsFlags::MS_NOEXEC,
            "noatime" => MsFlags::MS_NOATIME,
            "nodiratime" => MsFlags::MS_NODIRATIME,
            "relatime" => MsFlags::MS_RELATIME,
            _ => MsFlags::empty(),
        };
    }
    flags
}

fn unescape_mountinfo(s: &str) -> String {
    // mountinfo escapes space/tab/newline/backslash as octal (\040 etc.)
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.by_ref().take(3).collect();
            if let Ok(v) = u8::from_str_radix(&digits, 8) {
                out.push(v as char);
                continue;
            }
            out.push_str(&digits);
        } else {
            out.push(c);
        }
    }
    out
}

fn setup_dev(root: &Path) -> Result<(), String> {
    let dev = root.join("dev");
    mount_tmpfs(&dev, "mode=755")?;

    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let src = Path::new("/dev").join(node);
        if !src.exists() {
            continue;
        }
        let dst = dev.join(node);
        std::fs::File::create(&dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
        bind(&src, &dst, MsFlags::empty())?;
    }

    for (link, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ] {
        std::os::unix::fs::symlink(target, dev.join(link))
            .map_err(|e| format!("symlink /dev/{link}: {e}"))?;
    }

    let pts = dev.join("pts");
    mkdir(&pts)?;
    mount(
        Some("devpts"),
        &pts,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )
    .map_err(|e| format!("mount devpts: {e}"))?;

    let shm = dev.join("shm");
    mount_tmpfs(&shm, "mode=1777")?;
    Ok(())
}

/// A fresh sysfs shows only the sandbox's own (loopback-only) network
/// devices. The kernel only permits it when we own the net namespace, so
/// with `--network` (host netns) the sandbox simply gets an empty /sys.
fn setup_sys(root: &Path, host_network: bool) {
    let sys = root.join("sys");
    if mkdir(&sys).is_err() {
        return;
    }
    if host_network {
        return;
    }
    let _ = mount(
        Some("sysfs"),
        &sys,
        Some("sysfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_RDONLY,
        None::<&str>,
    );
}

fn mount_tmpfs(target: &Path, opts: &str) -> Result<(), String> {
    mkdir(target)?;
    mount(
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(opts),
    )
    .map_err(|e| format!("mount tmpfs at {}: {e}", target.display()))
}

fn bind(src: &Path, dst: &Path, extra: MsFlags) -> Result<(), String> {
    mount(
        Some(src),
        dst,
        None::<&str>,
        MsFlags::MS_BIND | extra,
        None::<&str>,
    )
    .map_err(|e| format!("bind {} -> {}: {e}", src.display(), dst.display()))
}

fn mkdir(p: &Path) -> Result<(), String> {
    match std::fs::create_dir_all(p) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("mkdir {}: {e}", p.display())),
    }
}

/// Bring `lo` up in the fresh net namespace via SIOCSIFFLAGS.
fn bring_up_loopback() -> Result<(), String> {
    // SAFETY: standard ifreq ioctl dance on an AF_INET dgram socket.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(format!("socket: {}", std::io::Error::last_os_error()));
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo\0".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("SIOCGIFFLAGS(lo): {e}"));
        }
        req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &req) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("SIOCSIFFLAGS(lo): {e}"));
        }
        libc::close(fd);
    }
    Ok(())
}
