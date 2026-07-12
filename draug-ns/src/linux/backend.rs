//! Host side of the namespace backend: spawn/exec/destroy over the setup
//! process and guest agent.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use draug_core::proto::{GuestMessage, HostMessage, MAX_FRAME_LEN};
use draug_core::{
    Backend, Error, ExecEvent, ExecHandle, ExecRequest, Registry, Result, Sandbox, SandboxId,
    SandboxSpec, SandboxState, SnapshotId,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use super::reexec::{CLEANUP_DIR_ENV, CONFIG_ENV, MODE_CLEANUP, MODE_SETUP, REEXEC_ENV};
use super::setup::{SetupConfig, SOCKET_NAME};
use super::{cgroup, uidmap};

const BACKEND_NAME: &str = "ns";
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);

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
}

#[async_trait]
impl Backend for NsBackend {
    async fn spawn(&self, spec: &SandboxSpec) -> Result<Sandbox> {
        let project_dir = validate_project_dir(&spec.rootfs)?;
        if spec.from_snapshot.is_some() {
            return Err(Error::Unsupported(
                "--from-snapshot is not implemented yet".into(),
            ));
        }

        let id = SandboxId(random_id()?);
        let state_dir = self.state_dir(&id);
        for sub in ["upper", "work", "rt", "root"] {
            std::fs::create_dir_all(state_dir.join(sub))
                .map_err(|e| Error::io("create state dir", e))?;
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

        match self.do_spawn(&sandbox, spec, &project_dir).await {
            Ok(sb) => Ok(sb),
            Err(e) => {
                // Roll back whatever half-exists; destroy is idempotent.
                let _ = self.destroy(&id).await;
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

    async fn snapshot(&self, _id: &SandboxId, _name: &str) -> Result<SnapshotId> {
        Err(Error::Unsupported(
            "snapshot is not implemented for the ns backend yet".into(),
        ))
    }

    async fn restore(&self, _id: &SandboxId, _snapshot: &SnapshotId) -> Result<()> {
        Err(Error::Unsupported(
            "restore is not implemented for the ns backend yet".into(),
        ))
    }

    async fn destroy(&self, id: &SandboxId) -> Result<()> {
        let key = id.0.clone();
        let sb = match self.reg(move |r| r.get_sandbox(&key)).await {
            Ok(sb) => sb,
            Err(Error::NotFound { .. }) => return Ok(()), // idempotent
            Err(e) => return Err(e),
        };

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
    if s.contains(',') || s.contains(':') {
        // These are separators in overlayfs mount options and there is no
        // portable escaping for them pre-5.12 (fsconfig).
        return Err(Error::InvalidSpec(format!(
            "project path {} contains ',' or ':', which overlayfs options cannot express",
            canon.display()
        )));
    }
    Ok(canon)
}

fn random_id() -> Result<String> {
    let mut buf = [0u8; 4];
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

// --- guest process management ------------------------------------------------

/// Read `(pid, starttime)` from the sandbox pidfile and verify the process
/// still exists with the same starttime (guards against pid reuse).
fn read_live_guest(state_dir: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(state_dir.join("guest.pid")).ok()?;
    let mut parts = content.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let recorded: u64 = parts.next()?.parse().ok()?;
    (proc_starttime(pid)? == recorded).then_some(pid)
}

fn guest_alive(state_dir: &Path) -> bool {
    read_live_guest(state_dir).is_some()
}

/// starttime (field 22) from /proc/pid/stat; None if the process is gone.
fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
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
        if proc_starttime(pid).is_none() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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
