//! Host side of the namespace backend: spawn/exec/destroy over the setup
//! process and guest agent.

use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use draug_core::proto::{GuestMessage, HostMessage, MAX_FRAME_LEN};
use draug_core::{
    Backend, Error, ExecEvent, ExecHandle, ExecRequest, Registry, Result, Sandbox, SandboxId,
    SandboxSpec, SandboxState, SnapshotId, SnapshotMeta,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use super::reexec::{
    CLEANUP_DIR_ENV, CONFIG_ENV, COPY_DST_ENV, COPY_SRC_ENV, MODE_CLEANUP, MODE_COPY, MODE_SETUP,
    REEXEC_ENV,
};
use super::setup::{SetupConfig, SOCKET_NAME};
use super::{cgroup, fscopy, uidmap};

const BACKEND_NAME: &str = "ns";
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Layer copies scale with the sandbox's writes; allow far more than setup.
const COPY_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct NsBackend {
    registry: Arc<Registry>,
    state_root: PathBuf,
}

impl NsBackend {
    /// `state_root` is where per-sandbox state dirs live, e.g.
    /// `$XDG_STATE_HOME/draug`.
    pub fn new(registry: Arc<Registry>, state_root: PathBuf) -> Self {
        Self {
            registry,
            state_root,
        }
    }

    /// Run a (cheap, synchronous) registry operation off the executor.
    async fn reg<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Registry) -> Result<T> + Send + 'static,
    {
        let registry = Arc::clone(&self.registry);
        tokio::task::spawn_blocking(move || f(&registry))
            .await
            .map_err(|e| Error::io("registry task", std::io::Error::other(e)))?
    }

    fn state_dir(&self, id: &SandboxId) -> PathBuf {
        self.state_root.join(&id.0)
    }

    fn snapshot_dir(&self, id: &SnapshotId) -> PathBuf {
        self.state_root.join("snapshots").join(&id.0)
    }

    /// Acquire the per-sandbox operation lock (blocking flock on
    /// `<state_dir>/op.lock`), off the executor. Held during
    /// spawn/snapshot/destroy so those operations cannot interleave — across
    /// tasks *and* across processes — and so reconciliation can tell a
    /// crashed sandbox (lock free) from one with an operation in flight.
    async fn acquire_op_lock(&self, state_dir: &Path) -> Result<OpLock> {
        let dir = state_dir.to_path_buf();
        tokio::task::spawn_blocking(move || flock_blocking(&dir))
            .await
            .map_err(|e| Error::io("op-lock task", std::io::Error::other(e)))?
            .map_err(|e| Error::io("acquire op lock", e))
    }

    /// Reconcile the registry against kernel liveness: reap sandboxes whose
    /// guest has died (crash/reboot) — freeing their cgroup, on-disk state,
    /// and row — and thaw any cgroup left frozen by a crash mid-snapshot.
    /// Skips sandboxes with an operation in flight (op-lock held), so it is
    /// safe to run at startup while other `sbx`/`mcp` processes are active.
    /// Returns the number of sandboxes reaped.
    pub async fn reconcile(&self) -> Result<usize> {
        let sandboxes = self.reg(|r| r.list_sandboxes()).await?;
        let mut reaped = 0;
        for sb in sandboxes {
            if sb.backend != BACKEND_NAME {
                continue; // not ours to reconcile
            }
            if guest_alive(&sb.state_dir) {
                self.thaw_if_orphaned(&sb).await;
                continue;
            }
            // No live guest: crashed, or a spawn is in flight elsewhere.
            if !sb.state_dir.exists() {
                // Row with no on-disk state: just drop the row.
                let key = sb.id.clone();
                let _ = self.reg(move |r| r.remove_sandbox(&key)).await;
                reaped += 1;
                continue;
            }
            let dir = sb.state_dir.clone();
            let lock = tokio::task::spawn_blocking(move || flock_try(&dir))
                .await
                .map_err(|e| Error::io("op-lock task", std::io::Error::other(e)))?;
            // Lock free (Ok(Some)) means no operation is in flight and the
            // guest is gone: reap. Held or unreadable: leave it be.
            if let Ok(Some(guard)) = lock {
                if self.destroy_inner(&sb).await.is_ok() {
                    reaped += 1;
                }
                drop(guard);
            }
        }
        Ok(reaped)
    }

    /// If a live sandbox's cgroup is frozen but no snapshot is in flight, a
    /// crash left it suspended — thaw it. Best-effort.
    async fn thaw_if_orphaned(&self, sb: &Sandbox) {
        let Some(cgpath) = read_cgroup_path(&sb.state_dir) else {
            return;
        };
        let dir = sb.state_dir.clone();
        let Ok(Ok(Some(guard))) = tokio::task::spawn_blocking(move || flock_try(&dir)).await else {
            return; // op in flight, or lock error: don't touch the freezer
        };
        let path = cgpath.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if cgroup::is_frozen(&path) {
                let _ = cgroup::unfreeze(&path);
            }
        })
        .await;
        drop(guard);
    }

    /// Structural diff of a layer against its base image, as
    /// added/modified/deleted paths. `target` is a snapshot (id or name) or
    /// a live sandbox (id or name): a snapshot diffs its captured upper
    /// layer, a sandbox diffs its live upper layer. Overlayfs whiteouts are
    /// decoded as deletions. Not on the `Backend` trait — it is a read-only
    /// inspection the CLI/MCP frontends call directly.
    pub async fn diff(&self, target: &str) -> Result<Vec<draug_core::DiffEntry>> {
        let (upper, base) = self.resolve_layer(target).await?;
        tokio::task::spawn_blocking(move || super::diff::diff_upper(&upper, &base))
            .await
            .map_err(|e| Error::io("diff task", std::io::Error::other(e)))?
    }

    /// Resolve a snapshot-or-sandbox reference to `(upper_layer, base_image)`.
    /// Snapshots win over sandboxes on an id/name clash (they are the more
    /// specific, immutable artifact).
    async fn resolve_layer(&self, target: &str) -> Result<(PathBuf, PathBuf)> {
        let key = target.to_owned();
        if let Ok(snap) = self.reg(move |r| r.get_snapshot(&key)).await {
            return Ok((snap.path.join("upper"), snap.rootfs));
        }
        let key = target.to_owned();
        let sb = self.reg(move |r| r.get_sandbox(&key)).await?;
        Ok((sb.state_dir.join("upper"), sb.rootfs))
    }
}

#[async_trait]
impl Backend for NsBackend {
    async fn spawn(&self, spec: &SandboxSpec) -> Result<Sandbox> {
        let project_dir = validate_project_dir(&spec.rootfs)?;

        let id = SandboxId(random_id()?);
        let state_dir = self.state_dir(&id);
        for sub in ["upper", "work", "rt", "root"] {
            std::fs::create_dir_all(state_dir.join(sub))
                .map_err(|e| Error::io("create state dir", e))?;
        }

        // Hold the op-lock for the whole spawn so a concurrent reconcile in
        // another process can't mistake this half-built sandbox (no guest pid
        // yet) for a crashed one and reap it out from under us.
        let op_lock = self.acquire_op_lock(&state_dir).await?;

        // Seed the writable layer from a snapshot's captured upper layer, if
        // asked. The snapshot's upper may hold subordinate-uid files and
        // whiteouts, so the copy runs through the userns copy helper.
        if let Some(snap_id) = &spec.from_snapshot {
            let key = snap_id.0.clone();
            let snap = self.reg(move |r| r.get_snapshot(&key)).await?;
            let src = snap.path.join("upper");
            let dst = state_dir.join("upper");
            copy_layer(&src, &dst).await.map_err(|e| {
                // spawn's caller rolls back via destroy; surface the cause.
                Error::io("seed upper from snapshot", std::io::Error::other(e.to_string()))
            })?;
        }

        let sandbox = Sandbox {
            id: id.clone(),
            name: spec.name.clone(),
            state: SandboxState::Creating,
            backend: BACKEND_NAME.into(),
            rootfs: project_dir.clone(),
            state_dir: state_dir.clone(),
            limits: spec.limits.clone(),
            created_at: unix_now(),
        };
        {
            let sb = sandbox.clone();
            self.reg(move |r| r.insert_sandbox(&sb)).await?;
        }

        let result = self.do_spawn(&sandbox, spec, &project_dir).await;
        match result {
            Ok(sb) => {
                drop(op_lock);
                Ok(sb)
            }
            Err(e) => {
                // Roll back whatever half-exists. We already hold the op-lock,
                // so clean up directly (calling `destroy`, which re-acquires
                // it, would self-deadlock on the same flock).
                let _ = self.destroy_inner(&sandbox).await;
                drop(op_lock);
                Err(e)
            }
        }
    }

    async fn exec(&self, id: &SandboxId, req: ExecRequest) -> Result<ExecHandle> {
        let key = id.0.clone();
        let sb = self.reg(move |r| r.get_sandbox(&key)).await?;
        if sb.state == SandboxState::Creating {
            return Err(Error::WrongState {
                id: sb.id.0,
                state: sb.state,
                op: "exec",
            });
        }
        if req.argv.is_empty() {
            return Err(Error::InvalidSpec("exec argv must not be empty".into()));
        }

        let sock = sb.state_dir.join("rt").join(SOCKET_NAME);
        // The socket lives in `rt/`, which is bind-mounted writable into the
        // sandbox. A process inside could unlink the socket and replace it
        // with a symlink so our connect(2) — which follows symlinks — lands on
        // an arbitrary host socket. Refuse if the path is not a real socket.
        match std::fs::symlink_metadata(&sock) {
            Ok(m) if m.file_type().is_socket() => {}
            Ok(_) => {
                return Err(Error::Protocol(format!(
                    "guest socket {} is not a socket (sandbox tampering?); refusing to connect",
                    sock.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(if guest_alive(&sb.state_dir) {
                    Error::Protocol("guest socket is missing while the guest is alive".into())
                } else {
                    Error::WrongState {
                        id: sb.id.0.clone(),
                        state: SandboxState::Stopped,
                        op: "exec",
                    }
                })
            }
            Err(e) => return Err(Error::io("stat guest socket", e)),
        }
        let mut stream = UnixStream::connect(&sock).await.map_err(|e| {
            if guest_alive(&sb.state_dir) {
                Error::Protocol(format!("guest socket unreachable: {e}"))
            } else {
                Error::WrongState {
                    id: sb.id.0.clone(),
                    state: SandboxState::Stopped,
                    op: "exec",
                }
            }
        })?;

        write_frame(
            &mut stream,
            &HostMessage::ExecRequest {
                argv: req.argv,
                env: req.env,
                cwd: req.cwd,
                stdin: false,
                tty: false,
                timeout_ms: req.timeout_ms,
            },
        )
        .await
        .map_err(|e| Error::Protocol(format!("send exec request: {e}")))?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let event = match read_frame::<GuestMessage>(&mut stream).await {
                    Ok(Some(GuestMessage::Started { pid })) => ExecEvent::Started { pid },
                    Ok(Some(GuestMessage::Stdout { data })) => ExecEvent::Stdout(data),
                    Ok(Some(GuestMessage::Stderr { data })) => ExecEvent::Stderr(data),
                    Ok(Some(GuestMessage::Exit { code, signal })) => {
                        let _ = tx.send(ExecEvent::Exited { code, signal }).await;
                        break;
                    }
                    Ok(Some(GuestMessage::Error { kind, message })) => {
                        let _ = tx.send(ExecEvent::Failed { kind, message }).await;
                        break;
                    }
                    Ok(None) => {
                        let _ = tx
                            .send(ExecEvent::Failed {
                                kind: "protocol".into(),
                                message: "guest closed the connection without a terminal frame"
                                    .into(),
                            })
                            .await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ExecEvent::Failed {
                                kind: "protocol".into(),
                                message: format!("reading guest stream: {e}"),
                            })
                            .await;
                        break;
                    }
                };
                if tx.send(event).await.is_err() {
                    // ExecHandle dropped: closing the connection is the
                    // cancellation signal; the guest kills the process group.
                    break;
                }
            }
        });
        Ok(ExecHandle::new(rx))
    }

    async fn snapshot(&self, id: &SandboxId, name: &str) -> Result<SnapshotId> {
        if name.trim().is_empty() {
            return Err(Error::InvalidSpec("snapshot name must not be empty".into()));
        }
        let key = id.0.clone();
        let sb = self.reg(move |r| r.get_sandbox(&key)).await?;

        // Serialize against destroy / concurrent snapshots on this sandbox,
        // then re-check the sandbox still exists (a destroy may have won the
        // lock and removed it) before copying its upper layer.
        let _op_lock = self.acquire_op_lock(&sb.state_dir).await?;
        let key = sb.id.0.clone();
        let sb = self.reg(move |r| r.get_sandbox(&key)).await?;

        let snap_id = SnapshotId(random_id()?);
        let snap_dir = self.snapshot_dir(&snap_id);
        let dst_upper = snap_dir.join("upper");
        // Create the snapshot dir exclusively: with 128-bit ids a collision is
        // vanishingly unlikely, but `create_new` guarantees we never adopt (or
        // later delete on rollback) a directory we didn't create here.
        if let Some(parent) = snap_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io("create snapshots dir", e))?;
        }
        match std::fs::create_dir(&snap_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyExists {
                    kind: draug_core::ResourceKind::Snapshot,
                    id: snap_id.0,
                })
            }
            Err(e) => return Err(Error::io("create snapshot dir", e)),
        }

        // Registry-first: record intent before touching the kernel, so a
        // crash mid-copy leaves a row (and its dir) that destroy/gc reclaims.
        let meta = SnapshotMeta {
            id: snap_id.clone(),
            sandbox_id: Some(sb.id.clone()),
            name: name.to_owned(),
            path: snap_dir.clone(),
            rootfs: sb.rootfs.clone(),
            size_bytes: 0,
            created_at: unix_now(),
        };
        {
            let m = meta.clone();
            if let Err(e) = self.reg(move |r| r.insert_snapshot(&m)).await {
                let _ = std::fs::remove_dir_all(&snap_dir);
                return Err(e);
            }
        }

        // Freeze the sandbox's processes so no writer mutates the upper layer
        // mid-copy, syncfs to flush the page cache, copy, then thaw. The
        // freeze is best-effort (a sandbox with no cgroup, or an already-dead
        // guest, simply skips it) but the copy always runs.
        let cg = read_cgroup_path(&sb.state_dir);
        let froze = match &cg {
            Some(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || cgroup::freeze(&path))
                    .await
                    .map_err(|e| Error::io("freeze task", std::io::Error::other(e)))?
                    .is_ok()
            }
            None => false,
        };

        let src_upper = sb.state_dir.join("upper");
        syncfs_dir(&src_upper).await;
        let result = copy_layer(&src_upper, &dst_upper).await;

        if froze {
            if let Some(path) = &cg {
                let path = path.clone();
                let _ = tokio::task::spawn_blocking(move || cgroup::unfreeze(&path)).await;
            }
        }

        match result {
            Ok(size) => {
                let sid = snap_id.clone();
                self.reg(move |r| r.set_snapshot_size(&sid, size)).await?;
                Ok(snap_id)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&snap_dir);
                let sid = snap_id.clone();
                let _ = self.reg(move |r| r.remove_snapshot(&sid)).await;
                Err(e)
            }
        }
    }

    async fn restore(&self, snapshot: &SnapshotId, name: Option<String>) -> Result<Sandbox> {
        let key = snapshot.0.clone();
        let snap = self.reg(move |r| r.get_snapshot(&key)).await?;

        // Restore materializes a fresh sandbox seeded from the snapshot's
        // captured layer, over the same base image. This never mutates the
        // snapshot and leaves any originating sandbox alone.
        let spec = SandboxSpec {
            name,
            rootfs: snap.rootfs.clone(),
            from_snapshot: Some(snap.id.clone()),
            limits: draug_core::ResourceLimits::unlimited(),
            env: vec![],
            network: false,
            allow_host_proc_fallback: false,
        };
        self.spawn(&spec).await
    }

    async fn destroy(&self, id: &SandboxId) -> Result<()> {
        let key = id.0.clone();
        let sb = match self.reg(move |r| r.get_sandbox(&key)).await {
            Ok(sb) => sb,
            Err(Error::NotFound { .. }) => return Ok(()), // idempotent
            Err(e) => return Err(e),
        };
        // Serialize against snapshot / concurrent destroy on this sandbox.
        // Best-effort: if the state dir is already gone there is nothing to
        // lock against, and destroy_inner is idempotent regardless.
        let _op_lock = self.acquire_op_lock(&sb.state_dir).await.ok();
        self.destroy_inner(&sb).await
    }
}

impl NsBackend {
    /// The teardown itself, without acquiring the op-lock (callers that
    /// already hold it — spawn rollback, reconcile — use this directly).
    /// Idempotent. `cgroup::destroy` writes `cgroup.kill`, which SIGKILLs even
    /// frozen tasks, so a sandbox left frozen by a crash is still torn down.
    async fn destroy_inner(&self, sb: &Sandbox) -> Result<()> {
        kill_guest(&sb.state_dir).await?;

        let cgroup_path_file = sb.state_dir.join("cgroup.path");
        if let Ok(path) = std::fs::read_to_string(&cgroup_path_file) {
            let path = PathBuf::from(path.trim());
            tokio::task::spawn_blocking(move || cgroup::destroy(&path))
                .await
                .ok();
        }

        remove_state_dir(&sb.state_dir).await?;

        let key = sb.id.clone();
        self.reg(move |r| r.remove_sandbox(&key)).await?;
        Ok(())
    }
}

impl NsBackend {
    async fn do_spawn(
        &self,
        sandbox: &Sandbox,
        spec: &SandboxSpec,
        project_dir: &Path,
    ) -> Result<Sandbox> {
        let state_dir = &sandbox.state_dir;

        let cg = {
            let (id, limits) = (sandbox.id.0.clone(), spec.limits.clone());
            tokio::task::spawn_blocking(move || cgroup::create(&id, &limits))
                .await
                .map_err(|e| Error::io("cgroup task", std::io::Error::other(e)))??
        };
        if let Some(cg) = &cg {
            std::fs::write(state_dir.join("cgroup.path"), cg.path.to_string_lossy().as_bytes())
                .map_err(|e| Error::io("record cgroup path", e))?;
        }

        let setup_cfg = SetupConfig {
            id: sandbox.id.0.clone(),
            state_dir: state_dir.clone(),
            project_dir: project_dir.to_path_buf(),
            hostname: spec
                .name
                .clone()
                .unwrap_or_else(|| format!("draug-{}", sandbox.id.0)),
            env: spec.env.clone(),
            network: spec.network,
            host_is_root: nix::unistd::geteuid().is_root(),
            allow_host_proc_fallback: spec.allow_host_proc_fallback,
        };
        let cfg_json = serde_json::to_string(&setup_cfg)
            .map_err(|e| Error::InvalidSpec(format!("unserializable spec: {e}")))?;

        let mut child = Command::new("/proc/self/exe")
            .env(REEXEC_ENV, MODE_SETUP)
            .env(CONFIG_ENV, cfg_json)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::io("spawn setup process", e))?;
        let setup_pid = child.id().ok_or_else(|| {
            Error::io("spawn setup process", std::io::Error::other("no pid"))
        })?;
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        // Collect setup's stderr in the background for diagnostics.
        let stderr_task = {
            let mut stderr = child.stderr.take().expect("piped");
            tokio::spawn(async move {
                let mut buf = String::new();
                let _ = stderr.read_to_string(&mut buf).await;
                buf
            })
        };

        let drive = async {
            let mut lines = BufReader::new(stdout).lines();
            let mut guest_pid: Option<u32> = None;
            loop {
                let Some(line) = lines
                    .next_line()
                    .await
                    .map_err(|e| Error::io("read setup output", e))?
                else {
                    let stderr = stderr_task.await.unwrap_or_default();
                    return Err(setup_error(format!(
                        "setup process exited before the guest was ready: {}",
                        stderr.trim()
                    )));
                };
                if line == "unshared" {
                    uidmap::write_maps(setup_pid)?;
                    if let Some(cg) = &cg {
                        cgroup::add_pid(cg, setup_pid)?;
                    }
                    stdin
                        .write_all(b"go\n")
                        .await
                        .map_err(|e| Error::io("signal setup process", e))?;
                } else if let Some(pid) = line.strip_prefix("pid ") {
                    guest_pid = pid.trim().parse().ok();
                } else if line == "ready" {
                    let pid = guest_pid
                        .ok_or_else(|| setup_error("guest ready before pid was reported".into()))?;
                    return Ok(pid);
                } else if let Some(msg) = line.strip_prefix("warn ") {
                    eprintln!("sbx: warning: {msg}");
                } else if let Some(msg) = line.strip_prefix("err ") {
                    return Err(setup_error(msg.into()));
                }
            }
        };
        let guest_pid = match tokio::time::timeout(SETUP_TIMEOUT, drive).await {
            Ok(Ok(pid)) => pid,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(e);
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(setup_error("sandbox setup timed out".into()));
            }
        };
        // The setup process exits right after reporting the guest pid.
        let _ = child.wait().await;

        let starttime = proc_starttime(guest_pid)
            .ok_or_else(|| setup_error("guest died immediately after setup".into()))?;
        std::fs::write(
            state_dir.join("guest.pid"),
            format!("{guest_pid} {starttime}\n"),
        )
        .map_err(|e| Error::io("write pidfile", e))?;

        let key = sandbox.id.clone();
        self.reg(move |r| r.update_sandbox_state(&key, SandboxState::Ready))
            .await?;
        Ok(Sandbox {
            state: SandboxState::Ready,
            ..sandbox.clone()
        })
    }
}

/// Classify a setup failure line into the error taxonomy.
fn setup_error(msg: String) -> Error {
    if msg.contains("CLONE_NEWUSER") {
        Error::Unsupported(format!(
            "{msg}. Check `sysctl kernel.unprivileged_userns_clone` (Debian-family), \
             `sysctl user.max_user_namespaces`, and any container seccomp policy"
        ))
    } else if msg.contains("overlay") {
        Error::Unsupported(msg)
    } else {
        Error::io("sandbox setup", std::io::Error::other(msg))
    }
}

fn validate_project_dir(p: &Path) -> Result<PathBuf> {
    let canon = p
        .canonicalize()
        .map_err(|e| Error::InvalidSpec(format!("project dir {}: {e}", p.display())))?;
    if !canon.is_dir() {
        return Err(Error::InvalidSpec(format!(
            "project path {} is not a directory",
            canon.display()
        )));
    }
    let s = canon.to_string_lossy();
    // `,` and `:` separate overlayfs mount options; `\` is the option-escape
    // char; control chars (incl. newline) can corrupt the option string or
    // mountinfo parsing. None have portable escaping pre-5.12 (fsconfig), so
    // reject any path that contains them.
    if let Some(bad) = s
        .chars()
        .find(|c| matches!(c, ',' | ':' | '\\') || c.is_control())
    {
        return Err(Error::InvalidSpec(format!(
            "project path {} contains {bad:?}, which cannot be expressed safely in \
             overlayfs mount options",
            canon.display()
        )));
    }
    Ok(canon)
}

/// A 128-bit random id, hex-encoded. Wide enough that collisions (which would
/// make two sandboxes/snapshots share a directory) are not a concern.
fn random_id() -> Result<String> {
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .map_err(|e| Error::io("read /dev/urandom", e))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- per-sandbox operation lock ----------------------------------------------

/// Guard for the per-sandbox operation lock. The flock is released when the
/// held `File` is closed on drop.
struct OpLock {
    _file: std::fs::File,
}

fn op_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("op.lock")
}

/// Acquire the op-lock, blocking until it is free. Creates the state dir and
/// lock file if needed. Blocking — call under `spawn_blocking`.
fn flock_blocking(state_dir: &Path) -> std::io::Result<OpLock> {
    std::fs::create_dir_all(state_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(op_lock_path(state_dir))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(OpLock { _file: file })
}

/// Try to acquire the op-lock without blocking. `Ok(None)` means it is held
/// by someone else (an operation is in flight) — or the state dir is gone.
/// Blocking file ops — call under `spawn_blocking`.
fn flock_try(state_dir: &Path) -> std::io::Result<Option<OpLock>> {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(op_lock_path(state_dir))
    {
        Ok(f) => f,
        // Parent dir vanished: nothing to lock; caller handles the missing dir.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } {
        0 => Ok(Some(OpLock { _file: file })),
        _ => {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

// --- layer copying (snapshot / restore) --------------------------------------

/// The sandbox cgroup path recorded at spawn time, if any.
fn read_cgroup_path(state_dir: &Path) -> Option<PathBuf> {
    let s = std::fs::read_to_string(state_dir.join("cgroup.path")).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Flush the filesystem backing `dir` so a subsequent copy sees committed
/// data. Best-effort: a missing dir or an fs that ignores syncfs is fine.
async fn syncfs_dir(dir: &Path) {
    let dir = dir.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(f) = std::fs::File::open(&dir) {
            use std::os::fd::AsRawFd;
            unsafe { libc::syncfs(f.as_raw_fd()) };
        }
    })
    .await;
}

/// Copy an overlay upper layer from `src` to `dst`, preserving whiteouts,
/// xattrs, and ownership. Tries a direct in-process copy first (succeeds as
/// real root, or when every file is owned by the caller); on a privilege
/// error — subordinate-uid files or whiteout device nodes we cannot recreate
/// unprivileged — retries inside a user namespace via the copy helper.
/// Returns the total regular-file bytes copied.
async fn copy_layer(src: &Path, dst: &Path) -> Result<u64> {
    let (s, d) = (src.to_path_buf(), dst.to_path_buf());
    let direct = tokio::task::spawn_blocking(move || fscopy::copy_tree(&s, &d))
        .await
        .map_err(|e| Error::io("copy task", std::io::Error::other(e)))?;
    match direct {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Partial output from the failed attempt must not corrupt the
            // helper's copy; the helper recreates the tree from scratch.
            let _ = tokio::fs::remove_dir_all(dst).await;
            copy_layer_via_helper(src, dst).await
        }
        Err(e) => Err(Error::io("copy layer", e)),
    }
}

/// Drive the re-exec'd copy helper: it enters a user namespace, we write the
/// spawn-time uid/gid maps, it copies and reports `done <bytes>`. Mirrors the
/// cleanup-helper line protocol.
async fn copy_layer_via_helper(src: &Path, dst: &Path) -> Result<u64> {
    let mut child = Command::new("/proc/self/exe")
        .env(REEXEC_ENV, MODE_COPY)
        .env(COPY_SRC_ENV, src.as_os_str())
        .env(COPY_DST_ENV, dst.as_os_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::io("spawn copy helper", e))?;
    let pid = child.id().unwrap_or_default();
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let drive = async {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| Error::io("copy helper", e))?
        {
            if line == "unshared" {
                uidmap::write_maps(pid)?;
                stdin
                    .write_all(b"go\n")
                    .await
                    .map_err(|e| Error::io("copy helper", e))?;
            } else if let Some(n) = line.strip_prefix("done ") {
                return n
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| Error::io("copy helper", std::io::Error::other(e)));
            } else if let Some(msg) = line.strip_prefix("err ") {
                return Err(Error::io("copy helper", std::io::Error::other(msg.to_owned())));
            }
        }
        Err(Error::io(
            "copy helper",
            std::io::Error::other("helper exited without confirming"),
        ))
    };
    let res = tokio::time::timeout(COPY_TIMEOUT, drive)
        .await
        .unwrap_or_else(|_| Err(Error::io("copy helper", std::io::Error::other("timed out"))));
    let _ = child.wait().await;
    res
}

// --- guest process management ------------------------------------------------

/// Read `(pid, starttime)` from the sandbox pidfile and verify the process
/// still exists with the same starttime (guards against pid reuse) and is not
/// a zombie. A killed guest reparented to a non-reaping ancestor lingers as a
/// zombie with a valid /proc entry; treating that as "alive" would wedge
/// reconcile, so a zombie counts as gone.
fn read_live_guest(state_dir: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(state_dir.join("guest.pid")).ok()?;
    let mut parts = content.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let recorded: u64 = parts.next()?.parse().ok()?;
    let (starttime, zombie) = proc_stat(pid)?;
    (starttime == recorded && !zombie).then_some(pid)
}

fn guest_alive(state_dir: &Path) -> bool {
    read_live_guest(state_dir).is_some()
}

/// starttime (field 22) from /proc/pid/stat; None if the process is gone.
fn proc_starttime(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|(starttime, _)| starttime)
}

/// `(starttime, is_zombie)` from /proc/pid/stat, or None if the process is
/// gone. starttime is field 22; the state char (field 3) is `Z` for a zombie.
fn proc_stat(pid: u32) -> Option<(u64, bool)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let zombie = fields.clone().next() == Some("Z");
    let starttime = fields.nth(19)?.parse().ok()?;
    Some((starttime, zombie))
}

/// SIGKILL the guest (PID 1 of the sandbox pidns — the kernel then kills
/// every process in the namespace) and wait for it to disappear.
async fn kill_guest(state_dir: &Path) -> Result<()> {
    let Some(pid) = read_live_guest(state_dir) else {
        return Ok(()); // never started or already gone
    };
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    for _ in 0..100 {
        // Gone, or a zombie awaiting reap by a non-reaping ancestor: either
        // way the guest is no longer running and its namespaces are torn down.
        match proc_stat(pid) {
            None | Some((_, true)) => return Ok(()),
            Some((_, false)) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    Err(Error::io(
        "kill sandbox",
        std::io::Error::other(format!("guest pid {pid} did not exit after SIGKILL")),
    ))
}

/// Remove the state dir. The upper layer can contain files owned by
/// subordinate uids, which we cannot unlink directly — in that case re-exec
/// a helper that re-enters a user namespace with the same mapping first.
async fn remove_state_dir(state_dir: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(state_dir).await {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(Error::io("remove state dir", e)),
    }

    let mut child = Command::new("/proc/self/exe")
        .env(REEXEC_ENV, MODE_CLEANUP)
        .env(CLEANUP_DIR_ENV, state_dir.as_os_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::io("spawn cleanup helper", e))?;
    let pid = child.id().unwrap_or_default();
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let drive = async {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| Error::io("cleanup helper", e))?
        {
            match line.as_str() {
                "unshared" => {
                    uidmap::write_maps(pid)?;
                    stdin
                        .write_all(b"go\n")
                        .await
                        .map_err(|e| Error::io("cleanup helper", e))?;
                }
                "done" => return Ok(()),
                other => {
                    if let Some(msg) = other.strip_prefix("err ") {
                        return Err(Error::io("cleanup helper", std::io::Error::other(msg)));
                    }
                }
            }
        }
        Err(Error::io(
            "cleanup helper",
            std::io::Error::other("helper exited without confirming"),
        ))
    };
    let res = tokio::time::timeout(SETUP_TIMEOUT, drive)
        .await
        .unwrap_or_else(|_| {
            Err(Error::io(
                "cleanup helper",
                std::io::Error::other("timed out"),
            ))
        });
    let _ = child.wait().await;
    res
}

// --- async framing -------------------------------------------------------------

async fn write_frame<T: Serialize>(stream: &mut UnixStream, msg: &T) -> std::io::Result<()> {
    let body = rmp_serde::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

async fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oversized frame from guest",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    rmp_serde::from_slice(&buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
