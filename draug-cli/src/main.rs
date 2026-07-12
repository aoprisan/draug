//! sbx: the draug CLI.

mod mcp;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use draug_core::{Backend, Error, ExecEvent, ExecRequest, Registry, ResourceLimits, SandboxSpec};
use draug_ns::NsBackend;

/// Exit code for sbx-internal failures (as opposed to the exec'd command's
/// own exit code, which is passed through).
const EXIT_INTERNAL: i32 = 125;
const EXIT_TIMEOUT: i32 = 124;
const EXIT_SPAWN_FAILED: i32 = 127;

#[derive(Parser)]
#[command(
    name = "sbx",
    about = "draug: local-first, zero-daemon sandboxes for AI coding agents",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create and start a sandbox around a project directory
    Run {
        /// Project directory (becomes the read-only overlay lower layer;
        /// writes land in the sandbox's upper layer)
        project_dir: PathBuf,
        /// Human-readable sandbox name (also its hostname)
        #[arg(long)]
        name: Option<String>,
        /// Memory ceiling in bytes (cgroup v2 memory.max)
        #[arg(long)]
        memory_max: Option<u64>,
        /// CPU budget in millicores; 1000 = one core (cgroup v2 cpu.max)
        #[arg(long)]
        cpu_millis: Option<u32>,
        /// Maximum number of processes (cgroup v2 pids.max)
        #[arg(long)]
        pids_max: Option<u32>,
        /// Give the sandbox the host network namespace (default: isolated
        /// netns with only loopback)
        #[arg(long)]
        network: bool,
        /// Extra environment for every exec, KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
    },
    /// Run a command inside a sandbox, streaming its output
    Exec {
        /// Sandbox id or name
        sandbox: String,
        /// Working directory inside the sandbox (default: the project dir)
        #[arg(long)]
        cwd: Option<String>,
        /// Kill the command after this many seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Extra environment, KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Command and arguments (no shell is implied)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// Capture a sandbox's filesystem state.
    /// Snapshots capture files, not processes: after restore, re-run
    /// whatever was running.
    Snapshot { sandbox: String, name: String },
    /// Materialize a new sandbox whose writable layer starts from a
    /// snapshot's captured contents (over the same base image). The
    /// snapshot and any originating sandbox are left untouched.
    Restore {
        /// Snapshot id or name to restore from
        snapshot: String,
        /// Name for the new sandbox
        #[arg(long)]
        name: Option<String>,
    },
    /// Show a layer's filesystem changes against its base image, as
    /// added/modified/deleted paths. The target is a snapshot or a live
    /// sandbox (id or name).
    Diff {
        /// Snapshot or sandbox to inspect
        target: String,
        /// Emit machine-readable JSON instead of a text summary
        #[arg(long)]
        json: bool,
    },
    /// Kill a sandbox's processes and delete all its state
    Destroy { sandbox: String },
    /// Serve the draug sandbox toolset as an MCP server over stdio
    /// (JSON-RPC 2.0), for use by LLM agents (e.g. Claude Code)
    Mcp {
        /// Host-side deadline applied to every tool call, in seconds
        #[arg(long, default_value_t = 300)]
        call_timeout: u64,
        /// Maximum number of sandboxes that may exist at once
        #[arg(long, default_value_t = 8)]
        max_sandboxes: usize,
    },
}

fn main() {
    // Must run before the tokio runtime exists: sandbox setup re-executes
    // this binary and forks.
    draug_ns::reexec::maybe_run();

    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}

fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/state")
        })
        .join("draug")
}

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>, String> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("--env {p:?} is not KEY=VALUE"))
        })
        .collect()
}

async fn run(cli: Cli) -> i32 {
    let state_root = state_root();
    let registry = match Registry::open(&state_root.join("registry.db")) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("sbx: cannot open registry: {e}");
            return EXIT_INTERNAL;
        }
    };
    let backend = NsBackend::new(Arc::clone(&registry), state_root);

    match dispatch(cli.command, &backend, &registry).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sbx: error: {e}");
            EXIT_INTERNAL
        }
    }
}

async fn dispatch(
    cmd: Command,
    backend: &NsBackend,
    registry: &Arc<Registry>,
) -> Result<i32, Error> {
    match cmd {
        Command::Run {
            project_dir,
            name,
            memory_max,
            cpu_millis,
            pids_max,
            network,
            env,
        } => {
            let env = parse_env(&env).map_err(Error::InvalidSpec)?;
            let spec = SandboxSpec {
                name,
                rootfs: project_dir,
                from_snapshot: None,
                limits: ResourceLimits {
                    cpu_millis,
                    memory_bytes: memory_max,
                    pids: pids_max,
                    disk_bytes: None,
                    wall_time: None,
                },
                env,
                network,
            };
            let sb = backend.spawn(&spec).await?;
            match &sb.name {
                Some(n) => eprintln!("sandbox {} ({n}) is ready", sb.id),
                None => eprintln!("sandbox {} is ready", sb.id),
            }
            println!("{}", sb.id);
            Ok(0)
        }
        Command::Exec {
            sandbox,
            cwd,
            timeout,
            env,
            argv,
        } => {
            let env = parse_env(&env).map_err(Error::InvalidSpec)?;
            let sb = registry.get_sandbox(&sandbox)?;
            let req = ExecRequest {
                argv,
                env,
                cwd,
                timeout_ms: timeout.map(|s| s * 1000),
            };
            let mut handle = backend.exec(&sb.id, req).await?;
            let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
            while let Some(event) = handle.next_event().await {
                match event {
                    ExecEvent::Started { .. } => {}
                    ExecEvent::Stdout(data) => {
                        let _ = out.write_all(&data).and_then(|_| out.flush());
                    }
                    ExecEvent::Stderr(data) => {
                        let _ = err.write_all(&data).and_then(|_| err.flush());
                    }
                    ExecEvent::Exited { code, signal } => {
                        return Ok(match (code, signal) {
                            (Some(c), _) => c,
                            (None, Some(sig)) => 128 + sig,
                            (None, None) => EXIT_INTERNAL,
                        });
                    }
                    ExecEvent::Failed { kind, message } => {
                        eprintln!("sbx: exec failed ({kind}): {message}");
                        return Ok(match kind.as_str() {
                            "timeout" => EXIT_TIMEOUT,
                            "spawn-failed" => EXIT_SPAWN_FAILED,
                            _ => EXIT_INTERNAL,
                        });
                    }
                }
            }
            eprintln!("sbx: exec stream ended unexpectedly");
            Ok(EXIT_INTERNAL)
        }
        Command::Destroy { sandbox } => {
            // Resolve names too, but stay idempotent for unknown ids.
            let id = match registry.get_sandbox(&sandbox) {
                Ok(sb) => sb.id,
                Err(Error::NotFound { .. }) => draug_core::SandboxId(sandbox),
                Err(e) => return Err(e),
            };
            backend.destroy(&id).await?;
            eprintln!("sandbox {id} destroyed");
            Ok(0)
        }
        Command::Snapshot { sandbox, name } => {
            let sb = registry.get_sandbox(&sandbox)?;
            let snap = backend.snapshot(&sb.id, &name).await?;
            eprintln!("snapshot {snap} ({name}) captured from {}", sb.id);
            println!("{snap}");
            Ok(0)
        }
        Command::Restore { snapshot, name } => {
            let snap = registry.get_snapshot(&snapshot)?;
            let sb = backend.restore(&snap.id, name).await?;
            match &sb.name {
                Some(n) => eprintln!("restored snapshot {} into sandbox {} ({n})", snap.id, sb.id),
                None => eprintln!("restored snapshot {} into sandbox {}", snap.id, sb.id),
            }
            println!("{}", sb.id);
            Ok(0)
        }
        Command::Diff { target, json } => {
            let entries = backend.diff(&target).await?;
            if json {
                let out = serde_json::to_string_pretty(&entries)
                    .map_err(|e| Error::io("serialize diff", std::io::Error::other(e)))?;
                println!("{out}");
            } else if entries.is_empty() {
                eprintln!("no changes against the base image");
            } else {
                for e in &entries {
                    let mark = match e.kind {
                        draug_core::DiffKind::Added => '+',
                        draug_core::DiffKind::Modified => '~',
                        draug_core::DiffKind::Deleted => '-',
                    };
                    match &e.size {
                        Some(size) => println!("{mark} {} ({size} bytes)", e.path),
                        None => println!("{mark} {}", e.path),
                    }
                }
            }
            Ok(0)
        }
        Command::Mcp {
            call_timeout,
            max_sandboxes,
        } => {
            let config = mcp::McpConfig {
                call_timeout: Duration::from_secs(call_timeout),
                max_sandboxes,
            };
            // Diagnostics only — stdout is the JSON-RPC channel.
            eprintln!(
                "sbx: MCP server on stdio (max {max_sandboxes} sandboxes, \
                 {call_timeout}s call timeout)"
            );
            mcp::serve(backend.clone(), Arc::clone(registry), config).await?;
            Ok(0)
        }
    }
}
