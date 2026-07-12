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
            let handle = self
                .rt
                .block_on(self.backend.exec(&self.sandbox.id, req))
                .unwrap_or_else(|e| panic!("exec failed: {e}"));
            self.rt.block_on(collect(handle))
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
