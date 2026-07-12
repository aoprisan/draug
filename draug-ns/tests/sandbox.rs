//! Integration tests for the namespace backend.
//!
//! `harness = false`: the backend re-executes the *current binary* for
//! namespace setup, so this test binary must dispatch re-exec modes before
//! running any tests. Tests skip (successfully) when the host cannot create
//! user namespaces — CI containers, non-Linux, locked-down kernels.

fn main() {
    // Never returns when this process was re-executed as a sandbox helper.
    draug_ns::reexec::maybe_run();

    #[cfg(not(target_os = "linux"))]
    println!("skipping draug-ns integration tests: not Linux");

    #[cfg(target_os = "linux")]
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use draug_core::{
        Backend, Error, ExecEvent, ExecHandle, ExecRequest, Registry, ResourceLimits, Sandbox,
        SandboxSpec,
    };
    use draug_ns::NsBackend;

    pub fn main() {
        if let Err(e) = draug_ns::userns_available() {
            println!("skipping draug-ns integration tests: {e}");
            return;
        }

        let tests: &[(&str, fn(&Env))] = &[
            ("exec_streams_stdout_stderr_and_exit_code", exec_streams),
            ("project_writes_stay_in_upper_layer", write_isolation),
            ("host_filesystem_is_read_only", host_read_only),
            ("network_is_loopback_only", network_isolated),
            ("hostname_is_set", hostname),
            ("signal_exit_is_reported", signal_exit),
            ("timeout_kills_the_command", timeout),
            ("spawn_failure_is_reported", spawn_failure),
            ("registry_lifecycle_and_destroy", lifecycle),
            ("snapshot_restore_roundtrip", snapshot_restore_roundtrip),
            ("diff_reports_whiteouts_as_deletions", diff_whiteouts),
            ("reconcile_reaps_dead_and_keeps_live", reconcile_reaps),
            ("exec_rejects_tampered_socket", tampered_socket),
        ];

        let env = Env::new();
        let mut failed = 0;
        for (name, test) in tests {
            print!("test {name} ... ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&env))) {
                Ok(()) => println!("ok"),
                Err(_) => {
                    println!("FAILED");
                    failed += 1;
                }
            }
        }
        drop(env);
        if failed > 0 {
            eprintln!("{failed} test(s) failed");
            std::process::exit(1);
        }
    }

    /// Shared fixture: one temp state root + registry + a single sandbox
    /// around a temp project dir (spawns are the expensive part).
    pub struct Env {
        _tmp: tempfile::TempDir,
        pub project: PathBuf,
        pub backend: NsBackend,
        pub registry: Arc<Registry>,
        pub rt: tokio::runtime::Runtime,
        pub sandbox: Sandbox,
    }

    impl Env {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            std::fs::write(project.join("hello.txt"), "hello draug\n").unwrap();

            let registry = Arc::new(Registry::open_in_memory().unwrap());
            let backend = NsBackend::new(Arc::clone(&registry), tmp.path().join("state"));
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            let sandbox = rt
                .block_on(backend.spawn(&spec(&project, Some("testbox"))))
                .unwrap_or_else(|e| panic!("spawn failed: {e}"));
            Self {
                _tmp: tmp,
                project,
                backend,
                registry,
                rt,
                sandbox,
            }
        }

        fn exec(&self, argv: &[&str]) -> ExecResult {
            self.exec_req(ExecRequest {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                env: vec![],
                cwd: None,
                timeout_ms: None,
            })
        }

        fn exec_req(&self, req: ExecRequest) -> ExecResult {
            self.exec_in(&self.sandbox.id, req)
        }

        /// Exec against an arbitrary sandbox (not just the shared fixture).
        fn exec_in(&self, id: &draug_core::SandboxId, req: ExecRequest) -> ExecResult {
            let handle = self
                .rt
                .block_on(self.backend.exec(id, req))
                .unwrap_or_else(|e| panic!("exec failed: {e}"));
            self.rt.block_on(collect(handle))
        }

        fn sh_in(&self, id: &draug_core::SandboxId, script: &str) -> ExecResult {
            self.exec_in(
                id,
                ExecRequest {
                    argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                    env: vec![],
                    cwd: None,
                    timeout_ms: None,
                },
            )
        }

        /// Spawn a throwaway sandbox around a fresh project dir seeded with
        /// `files` (name, contents). Returns (sandbox, project path).
        fn fresh_sandbox(&self, tag: &str, files: &[(&str, &str)]) -> (Sandbox, PathBuf) {
            let project = self.project.parent().unwrap().join(tag);
            std::fs::create_dir_all(&project).unwrap();
            for (name, contents) in files {
                std::fs::write(project.join(name), contents).unwrap();
            }
            let sb = self
                .rt
                .block_on(self.backend.spawn(&spec(&project, None)))
                .unwrap_or_else(|e| panic!("spawn {tag} failed: {e}"));
            (sb, project)
        }
    }

    fn spec(project: &Path, name: Option<&str>) -> SandboxSpec {
        SandboxSpec {
            name: name.map(str::to_string),
            rootfs: project.to_path_buf(),
            from_snapshot: None,
            limits: ResourceLimits::unlimited(),
            env: vec![("DRAUG_TEST_MARKER".into(), "1".into())],
            network: false,
            allow_host_proc_fallback: false,
        }
    }

    #[derive(Debug, Default)]
    struct ExecResult {
        stdout: String,
        stderr: String,
        code: Option<i32>,
        signal: Option<i32>,
        failed: Option<(String, String)>,
    }

    async fn collect(mut handle: ExecHandle) -> ExecResult {
        let mut r = ExecResult::default();
        while let Some(ev) = handle.next_event().await {
            match ev {
                ExecEvent::Started { .. } => {}
                ExecEvent::Stdout(d) => r.stdout.push_str(&String::from_utf8_lossy(&d)),
                ExecEvent::Stderr(d) => r.stderr.push_str(&String::from_utf8_lossy(&d)),
                ExecEvent::Exited { code, signal } => {
                    r.code = code;
                    r.signal = signal;
                    return r;
                }
                ExecEvent::Failed { kind, message } => {
                    r.failed = Some((kind, message));
                    return r;
                }
            }
        }
        panic!("exec stream ended without a terminal event");
    }

    // --- tests ---------------------------------------------------------------

    fn exec_streams(env: &Env) {
        // cwd defaults to the project dir.
        let r = env.exec(&["cat", "hello.txt"]);
        assert_eq!(r.stdout, "hello draug\n", "stderr: {}", r.stderr);
        assert_eq!(r.code, Some(0));

        let r = env.exec(&["/bin/sh", "-c", "echo out; echo err >&2; exit 3"]);
        assert_eq!(r.stdout, "out\n");
        assert_eq!(r.stderr, "err\n");
        assert_eq!(r.code, Some(3));

        // Sandbox env from the spec is present.
        let r = env.exec(&["/bin/sh", "-c", "echo -n $DRAUG_TEST_MARKER"]);
        assert_eq!(r.stdout, "1");
    }

    fn write_isolation(env: &Env) {
        let r = env.exec(&["/bin/sh", "-c", "echo sandboxed > created.txt"]);
        assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);

        // Visible inside...
        let r = env.exec(&["cat", "created.txt"]);
        assert_eq!(r.stdout, "sandboxed\n");

        // ...but never in the real project dir on the host.
        assert!(
            !env.project.join("created.txt").exists(),
            "sandbox write leaked into the host project dir"
        );

        // Deleting a lower-layer file works (whiteout) without touching it.
        let r = env.exec(&["rm", "hello.txt"]);
        assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
        let r = env.exec(&["cat", "hello.txt"]);
        assert_ne!(r.code, Some(0));
        assert!(env.project.join("hello.txt").exists());
    }

    fn host_read_only(env: &Env) {
        // System dirs are bound read-only.
        let r = env.exec(&["/bin/sh", "-c", "touch /usr/draug-was-here 2>&1"]);
        assert_ne!(r.code, Some(0), "wrote into /usr! stdout: {}", r.stdout);
        assert!(
            r.stdout.contains("Read-only") || r.stdout.contains("read-only"),
            "expected a read-only error, got: {} {}",
            r.stdout,
            r.stderr
        );

        // User-writable host locations simply don't exist inside.
        let r = env.exec(&["/bin/sh", "-c", "ls /home 2>/dev/null | wc -l"]);
        assert_eq!(r.stdout.trim(), "0", "/home should be absent or empty");
    }

    fn network_isolated(env: &Env) {
        let r = env.exec(&["cat", "/proc/net/dev"]);
        assert_eq!(r.code, Some(0));
        assert!(r.stdout.contains("lo:"), "loopback missing: {}", r.stdout);
        let ifaces: Vec<&str> = r
            .stdout
            .lines()
            .skip(2)
            .filter_map(|l| l.split(':').next())
            .map(str::trim)
            .collect();
        assert_eq!(ifaces, vec!["lo"], "unexpected interfaces: {ifaces:?}");
    }

    fn hostname(env: &Env) {
        let r = env.exec(&["cat", "/proc/sys/kernel/hostname"]);
        assert_eq!(r.stdout.trim(), "testbox");
    }

    fn signal_exit(env: &Env) {
        let r = env.exec(&["/bin/sh", "-c", "kill -9 $$"]);
        assert_eq!(r.code, None);
        assert_eq!(r.signal, Some(9));
    }

    fn timeout(env: &Env) {
        let start = std::time::Instant::now();
        let r = env.exec_req(ExecRequest {
            argv: vec!["sleep".into(), "30".into()],
            env: vec![],
            cwd: None,
            timeout_ms: Some(400),
        });
        let (kind, _) = r.failed.expect("expected a Failed event");
        assert_eq!(kind, "timeout");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "timeout took too long"
        );
    }

    fn spawn_failure(env: &Env) {
        let r = env.exec(&["/no/such/binary"]);
        let (kind, msg) = r.failed.expect("expected a Failed event");
        assert_eq!(kind, "spawn-failed");
        assert!(msg.contains("/no/such/binary"), "message: {msg}");
    }

    fn snapshot_restore_roundtrip(env: &Env) {
        // A sandbox whose base image carries `base.txt`.
        let (sb, _project) = env.fresh_sandbox("snap_project", &[("base.txt", "base\n")]);

        // Write into the upper layer: a new file, plus an edit shadowing the
        // base file.
        assert_eq!(
            env.sh_in(&sb.id, "echo captured > added.txt; echo edited > base.txt")
                .code,
            Some(0)
        );

        // Snapshot the sandbox at this state.
        let snap = env
            .rt
            .block_on(env.backend.snapshot(&sb.id, "checkpoint"))
            .unwrap_or_else(|e| panic!("snapshot failed: {e}"));

        // Mutate further: remove the added file, change the edit, add another.
        assert_eq!(
            env.sh_in(&sb.id, "rm added.txt; echo mutated > base.txt; echo late > late.txt")
                .code,
            Some(0)
        );
        assert_eq!(env.sh_in(&sb.id, "cat base.txt").stdout, "mutated\n");

        // Restore materializes a NEW sandbox from the snapshot.
        let restored = env
            .rt
            .block_on(env.backend.restore(&snap, Some("restored-box".into())))
            .unwrap_or_else(|e| panic!("restore failed: {e}"));
        assert_ne!(restored.id, sb.id, "restore must create a new sandbox");

        // Restored state matches the snapshot, not the later mutations.
        assert_eq!(
            env.sh_in(&restored.id, "cat added.txt").stdout,
            "captured\n",
            "snapshot-era file missing after restore"
        );
        assert_eq!(
            env.sh_in(&restored.id, "cat base.txt").stdout,
            "edited\n",
            "restored file has post-snapshot contents"
        );
        let late = env.sh_in(&restored.id, "cat late.txt 2>/dev/null; true");
        assert_eq!(late.stdout, "", "post-snapshot file leaked into restore");

        // The originating sandbox is untouched by the restore.
        assert_eq!(env.sh_in(&sb.id, "cat base.txt").stdout, "mutated\n");

        env.rt.block_on(env.backend.destroy(&sb.id)).unwrap();
        env.rt.block_on(env.backend.destroy(&restored.id)).unwrap();
    }

    fn diff_whiteouts(env: &Env) {
        use draug_core::DiffKind;

        let (sb, _project) = env.fresh_sandbox(
            "diff_project",
            &[("keep.txt", "keep\n"), ("remove.txt", "remove\n")],
        );

        // Add a file, modify a base file, and delete a base file (the delete
        // becomes an overlayfs whiteout in the upper layer).
        assert_eq!(
            env.sh_in(
                &sb.id,
                "echo new > fresh.txt; echo changed > keep.txt; rm remove.txt"
            )
            .code,
            Some(0)
        );

        let entries = env
            .rt
            .block_on(env.backend.diff(&sb.id.0))
            .unwrap_or_else(|e| panic!("diff failed: {e}"));

        let kind = |p: &str| entries.iter().find(|e| e.path == p).map(|e| e.kind);
        assert_eq!(kind("fresh.txt"), Some(DiffKind::Added), "entries: {entries:?}");
        assert_eq!(kind("keep.txt"), Some(DiffKind::Modified), "entries: {entries:?}");
        assert_eq!(
            kind("remove.txt"),
            Some(DiffKind::Deleted),
            "whiteout not reported as a deletion: {entries:?}"
        );

        // Deleted entries carry the base file's size/mode, not the upper's.
        let removed = entries.iter().find(|e| e.path == "remove.txt").unwrap();
        assert_eq!(removed.size, Some("remove\n".len() as u64));
        assert!(removed.mode.as_deref().is_some_and(|m| m.ends_with("644")));

        // Diffing the snapshot of that state reports the same deletion.
        let snap = env
            .rt
            .block_on(env.backend.snapshot(&sb.id, "diff-snap"))
            .unwrap();
        let snap_entries = env.rt.block_on(env.backend.diff(&snap.0)).unwrap();
        assert_eq!(
            snap_entries.iter().find(|e| e.path == "remove.txt").map(|e| e.kind),
            Some(DiffKind::Deleted),
            "snapshot diff lost the whiteout: {snap_entries:?}"
        );

        env.rt.block_on(env.backend.destroy(&sb.id)).unwrap();
    }

    fn guest_pid(state_dir: &Path) -> i32 {
        let content = std::fs::read_to_string(state_dir.join("guest.pid")).unwrap();
        content.split_whitespace().next().unwrap().parse().unwrap()
    }

    fn wait_gone(pid: i32) {
        for _ in 0..200 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("guest pid {pid} did not exit after SIGKILL");
    }

    fn reconcile_reaps(env: &Env) {
        use draug_core::Error;

        let (sb, _project) = env.fresh_sandbox("reap_project", &[("f.txt", "1\n")]);

        // A LIVE sandbox must survive reconcile untouched.
        let reaped = env.rt.block_on(env.backend.reconcile()).unwrap();
        assert!(
            env.registry.get_sandbox(&sb.id.0).is_ok(),
            "reconcile reaped a live sandbox (reaped {reaped})"
        );
        assert!(sb.state_dir.exists());
        // ...and it still works.
        assert_eq!(env.sh_in(&sb.id, "cat f.txt").stdout, "1\n");

        // Simulate a host crash: SIGKILL the guest (PID 1 of the sandbox), so
        // the row + on-disk state remain but the guest is gone.
        let pid = guest_pid(&sb.state_dir);
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        wait_gone(pid);

        let reaped = env.rt.block_on(env.backend.reconcile()).unwrap();
        assert!(reaped >= 1, "reconcile did not reap the dead sandbox");
        assert!(
            matches!(env.registry.get_sandbox(&sb.id.0), Err(Error::NotFound { .. })),
            "dead sandbox row survived reconcile"
        );
        assert!(
            !sb.state_dir.exists(),
            "dead sandbox state dir survived reconcile: {}",
            sb.state_dir.display()
        );
    }

    fn tampered_socket(env: &Env) {
        use draug_core::Error;

        let (sb, _project) = env.fresh_sandbox("tamper_project", &[("f.txt", "1\n")]);
        // Replace the guest socket with a symlink, as a malicious process
        // inside the sandbox could (rt/ is bind-mounted writable).
        let sock = sb.state_dir.join("rt").join("guest.sock");
        std::fs::remove_file(&sock).unwrap();
        std::os::unix::fs::symlink("/tmp/draug-does-not-exist.sock", &sock).unwrap();

        let res = env.rt.block_on(env.backend.exec(
            &sb.id,
            ExecRequest {
                argv: vec!["true".into()],
                env: vec![],
                cwd: None,
                timeout_ms: None,
            },
        ));
        let refused = match res {
            Err(Error::Protocol(_)) => true,
            Err(other) => panic!("expected Protocol error, got {other}"),
            Ok(_) => false,
        };
        assert!(
            refused,
            "exec followed a tampered (symlinked) socket instead of refusing"
        );

        env.rt.block_on(env.backend.destroy(&sb.id)).unwrap();
    }

    fn lifecycle(env: &Env) {
        use draug_core::SandboxState;

        // The fixture sandbox is registered and Ready, findable by name.
        let sb = env.registry.get_sandbox("testbox").unwrap();
        assert_eq!(sb.state, SandboxState::Ready);
        assert_eq!(sb.id, env.sandbox.id);

        // Spawn a second sandbox just to destroy it.
        let project2 = env.project.parent().unwrap().join("project2");
        std::fs::create_dir_all(&project2).unwrap();
        let sb2 = env
            .rt
            .block_on(env.backend.spawn(&spec(&project2, None)))
            .unwrap();
        assert!(sb2.state_dir.exists());

        env.rt.block_on(env.backend.destroy(&sb2.id)).unwrap();
        assert!(
            !sb2.state_dir.exists(),
            "state dir survived destroy: {}",
            sb2.state_dir.display()
        );
        assert!(matches!(
            env.registry.get_sandbox(&sb2.id.0),
            Err(Error::NotFound { .. })
        ));
        // Idempotent.
        env.rt.block_on(env.backend.destroy(&sb2.id)).unwrap();
    }
}
