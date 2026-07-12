//! The guest exec server: sync, thread-per-connection, no async runtime.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use draug_proto::sync_io::{read_frame, write_frame};
use draug_proto::{error_kind, GuestMessage, HostMessage, MAX_CHUNK_LEN};
use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

use crate::GuestConfig;

pub fn guest_main(cfg: GuestConfig) -> ! {
    match run(cfg) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("draug-guest: fatal: {e}");
            std::process::exit(1);
        }
    }
}

fn run(cfg: GuestConfig) -> io::Result<()> {
    if cfg.mount_proc {
        // We are PID 1 of a fresh pid namespace; /proc must be mounted from
        // inside it for anything (including our own reaping) to make sense.
        nix::mount::mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            nix::mount::MsFlags::MS_NOSUID
                | nix::mount::MsFlags::MS_NODEV
                | nix::mount::MsFlags::MS_NOEXEC,
            None::<&str>,
        )
        .map_err(|e| io::Error::other(format!("mount /proc: {e}")))?;
    }

    let _ = std::fs::remove_file(&cfg.socket_path);
    let listener = UnixListener::bind(&cfg.socket_path)?;

    // Tell the host we're accepting connections (stdout is still the setup
    // pipe at this point), then point our own stdio at the log file.
    println!("ready");
    io::stdout().flush()?;
    redirect_stdio(&cfg)?;

    std::thread::spawn(reaper);

    let cfg = Arc::new(cfg);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cfg = Arc::clone(&cfg);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &cfg) {
                        eprintln!("draug-guest: conn: {e}");
                    }
                });
            }
            Err(e) => eprintln!("draug-guest: accept: {e}"),
        }
    }
    Ok(())
}

fn redirect_stdio(cfg: &GuestConfig) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log_path)?;
    let devnull = std::fs::File::open("/dev/null")?;
    // SAFETY: plain dup2 onto our own stdio fds.
    unsafe {
        if libc::dup2(devnull.as_raw_fd(), 0) < 0
            || libc::dup2(log.as_raw_fd(), 1) < 0
            || libc::dup2(log.as_raw_fd(), 2) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// --- child reaping ---------------------------------------------------------
//
// As PID 1 we must reap everything, including orphans reparented to us, but
// exec connections need the exit status of *their* child. A single reaper
// thread does all waitpid(-1) calls and routes statuses; statuses that
// arrive before the exec thread registers are parked in `orphans`.

#[derive(Debug, Clone, Copy)]
struct ExitInfo {
    code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Default)]
struct Router {
    waiters: HashMap<i32, SyncSender<ExitInfo>>,
    orphans: HashMap<i32, ExitInfo>,
}

fn router() -> &'static Mutex<Router> {
    static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
    ROUTER.get_or_init(|| Mutex::new(Router::default()))
}

fn reaper() {
    loop {
        match waitpid(None, Some(WaitPidFlag::__WALL)) {
            Ok(WaitStatus::Exited(pid, code)) => route(
                pid.as_raw(),
                ExitInfo {
                    code: Some(code),
                    signal: None,
                },
            ),
            Ok(WaitStatus::Signaled(pid, sig, _)) => route(
                pid.as_raw(),
                ExitInfo {
                    code: None,
                    signal: Some(sig as i32),
                },
            ),
            Ok(_) => {}
            Err(Errno::ECHILD) => std::thread::sleep(Duration::from_millis(50)),
            Err(Errno::EINTR) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn route(pid: i32, info: ExitInfo) {
    let mut r = router().lock().unwrap();
    match r.waiters.remove(&pid) {
        Some(tx) => {
            let _ = tx.send(info);
        }
        None => {
            // Either an exec thread that hasn't registered yet, or a true
            // orphan. Exec threads clean their entry up; true orphans leak a
            // map entry of a few bytes, which is acceptable for now.
            r.orphans.insert(pid, info);
        }
    }
}

/// Block until the reaper delivers `pid`'s exit status.
fn wait_child(pid: i32) -> ExitInfo {
    let rx = {
        let mut r = router().lock().unwrap();
        if let Some(info) = r.orphans.remove(&pid) {
            return info;
        }
        let (tx, rx) = sync_channel(1);
        r.waiters.insert(pid, tx);
        rx
    };
    rx.recv().expect("reaper thread died")
}

// --- exec connections --------------------------------------------------------

fn protocol_error(stream: &Mutex<UnixStream>, message: &str) -> io::Result<()> {
    write_frame(
        &mut *stream.lock().unwrap(),
        &GuestMessage::Error {
            kind: error_kind::PROTOCOL.into(),
            message: message.into(),
        },
    )
}

fn handle_conn(stream: UnixStream, cfg: &GuestConfig) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));

    let (argv, env, cwd, want_stdin, tty, timeout_ms) = match read_frame(&mut reader)? {
        Some(HostMessage::ExecRequest {
            argv,
            env,
            cwd,
            stdin,
            tty,
            timeout_ms,
        }) => (argv, env, cwd, stdin, tty, timeout_ms),
        Some(_) => return protocol_error(&writer, "first frame must be ExecRequest"),
        None => return Ok(()), // connect-then-close probe
    };
    if argv.is_empty() {
        return protocol_error(&writer, "empty argv");
    }
    if tty {
        return protocol_error(&writer, "tty allocation not implemented yet");
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(
            cwd.map(std::path::PathBuf::from)
                .unwrap_or_else(|| cfg.default_cwd.clone()),
        )
        .stdin(if want_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own session + process group so cancellation/timeout can kill the
    // whole tree with one kill(-pgid).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{}: {e}", argv[0]);
            return write_frame(
                &mut *writer.lock().unwrap(),
                &GuestMessage::Error {
                    kind: error_kind::SPAWN_FAILED.into(),
                    message: msg,
                },
            );
        }
    };
    let pid = child.id() as i32;
    write_frame(&mut *writer.lock().unwrap(), &GuestMessage::Started { pid: pid as u32 })?;

    let mut child_stdin = child.stdin.take();
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    // The reaper owns waiting; `child` must never be wait()ed on again.
    drop(child);

    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));

    let out_pump = spawn_pump(stdout_pipe, Arc::clone(&writer), true);
    let err_pump = spawn_pump(stderr_pipe, Arc::clone(&writer), false);

    // Watchdog: enforce the deadline guest-side so a wedged host can't leave
    // runaway processes behind.
    let (done_tx, done_rx) = sync_channel::<()>(1);
    if let Some(ms) = timeout_ms {
        let timed_out = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            if done_rx.recv_timeout(Duration::from_millis(ms)).is_err() {
                timed_out.store(true, Ordering::SeqCst);
                kill_group(pid);
            }
        });
    }

    // Completion thread: deliver the terminal frame, then shut the socket
    // down so the stdin/cancel loop below unblocks.
    let completion = {
        let writer = Arc::clone(&writer);
        let done = Arc::clone(&done);
        let timed_out = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            let info = wait_child(pid);
            done.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
            // Flush remaining output before the terminal frame.
            let _ = out_pump.join();
            let _ = err_pump.join();
            let terminal = if timed_out.load(Ordering::SeqCst) {
                GuestMessage::Error {
                    kind: error_kind::TIMEOUT.into(),
                    message: "deadline exceeded; process group killed".into(),
                }
            } else {
                GuestMessage::Exit {
                    code: info.code,
                    signal: info.signal,
                }
            };
            let mut w = writer.lock().unwrap();
            let _ = write_frame(&mut *w, &terminal);
            let _ = w.shutdown(std::net::Shutdown::Both);
        })
    };

    // Stdin / cancellation loop. Host closing the connection before the
    // terminal frame means cancel: kill the process group.
    loop {
        match read_frame::<HostMessage>(&mut reader) {
            Ok(Some(HostMessage::Stdin { data })) => {
                if data.is_empty() {
                    child_stdin = None; // half-close
                } else if let Some(stdin) = child_stdin.as_mut() {
                    if stdin.write_all(&data).is_err() {
                        child_stdin = None;
                    }
                }
            }
            Ok(Some(_)) | Ok(None) | Err(_) => break,
        }
    }
    if !done.load(Ordering::SeqCst) {
        kill_group(pid);
    }
    let _ = completion.join();
    Ok(())
}

fn spawn_pump(
    mut pipe: impl Read + Send + 'static,
    writer: Arc<Mutex<UnixStream>>,
    is_stdout: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; MAX_CHUNK_LEN];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let msg = if is_stdout {
                        GuestMessage::Stdout { data }
                    } else {
                        GuestMessage::Stderr { data }
                    };
                    if write_frame(&mut *writer.lock().unwrap(), &msg).is_err() {
                        break; // host gone; cancel path will clean up
                    }
                }
            }
        }
    })
}

fn kill_group(pid: i32) {
    // setsid in pre_exec makes pgid == pid. ESRCH just means it's gone.
    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
}
