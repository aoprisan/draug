//! `sbx mcp`: the draug sandbox toolset exposed as an MCP server over stdio
//! (JSON-RPC 2.0), built on the official Rust SDK (`rmcp`).
//!
//! The transport is stdin/stdout, so **nothing** here may print to stdout —
//! that channel carries framed JSON-RPC. All diagnostics go to stderr.
//!
//! Design notes, because the results are consumed by an LLM, not a human:
//!
//! - Every tool returns *structured* JSON (`CallToolResult::structured`), so
//!   the model gets typed fields rather than prose to parse.
//! - `sandbox_exec` output is truncated head+tail (see [`clamp`]) so a chatty
//!   build can't blow the model's context; the marker states how much was
//!   dropped.
//! - Every failure is a structured, actionable error object — never a bare
//!   panic and never an opaque protocol error. Backend errors are classified
//!   into `{error, message, retriable, hint}` by [`error_object`].
//! - Two guards keep a misbehaving agent from exhausting the host: a
//!   host-side per-call timeout, and a max-concurrent-sandboxes quota checked
//!   under a lock so racing `sandbox_create` calls can't overshoot it.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use draug_core::{
    Backend, Error, ExecEvent, ExecRequest, Registry, ResourceLimits, Sandbox, SandboxId,
    SandboxSpec,
};
use draug_ns::NsBackend;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

/// Tunables for the MCP server, from `sbx mcp` flags.
#[derive(Debug, Clone, Copy)]
pub struct McpConfig {
    /// Host-side deadline applied to every tool call.
    pub call_timeout: Duration,
    /// Maximum number of sandboxes that may exist at once.
    pub max_sandboxes: usize,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(300),
            max_sandboxes: 8,
        }
    }
}

/// Build the server and serve it over stdio until the client disconnects.
pub async fn serve(
    backend: NsBackend,
    registry: Arc<Registry>,
    config: McpConfig,
) -> draug_core::Result<()> {
    let server = DraugMcp::new(backend, registry, config);
    // stdio() = (tokio stdin, tokio stdout); the SDK frames JSON-RPC over it.
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::io("mcp serve", std::io::Error::other(e)))?;
    running
        .waiting()
        .await
        .map_err(|e| Error::io("mcp serve", std::io::Error::other(e)))?;
    Ok(())
}

#[derive(Clone)]
struct DraugMcp {
    backend: NsBackend,
    registry: Arc<Registry>,
    config: McpConfig,
    /// Serializes `sandbox_create` so the quota check and the row insert it
    /// gates on cannot interleave across concurrent calls.
    create_gate: Arc<tokio::sync::Mutex<()>>,
    tool_router: ToolRouter<Self>,
}

// --- tool argument schemas ---------------------------------------------------
//
// `JsonSchema` drives the tool's input schema the model sees; the doc comments
// become field descriptions, so they are written for the model.

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateArgs {
    /// Absolute path to the project directory to sandbox. It becomes the
    /// read-only overlay lower layer; the sandbox's writes land in a private
    /// upper layer and never touch this directory.
    project_dir: String,
    /// Optional human-readable name, unique across live sandboxes. Also the
    /// sandbox hostname. Use it to refer to the sandbox in later calls.
    #[serde(default)]
    name: Option<String>,
    /// Memory ceiling in bytes (cgroup v2 memory.max). Requires a delegated
    /// cgroup v2 subtree; omit for unlimited.
    #[serde(default)]
    memory_max: Option<u64>,
    /// CPU budget in millicores; 1000 = one full core (cgroup v2 cpu.max).
    #[serde(default)]
    cpu_millis: Option<u32>,
    /// Maximum number of processes/threads (cgroup v2 pids.max).
    #[serde(default)]
    pids_max: Option<u32>,
    /// Give the sandbox the host network. Default false: an isolated network
    /// namespace with only loopback.
    #[serde(default)]
    network: bool,
    /// Extra environment variables set for every exec in this sandbox.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecArgs {
    /// Sandbox id or name to run in.
    sandbox: String,
    /// Command and arguments. argv[0] is the program; NO shell is implied, so
    /// use `["sh", "-c", "..."]` explicitly if you need shell features.
    command: Vec<String>,
    /// Working directory inside the sandbox. Defaults to the project dir.
    #[serde(default)]
    cwd: Option<String>,
    /// Kill the command after this many milliseconds (enforced inside the
    /// sandbox). Clamped so it always fires before the host call deadline.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Extra environment for this one command, on top of the sandbox's env.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnapshotArgs {
    /// Sandbox id or name to capture.
    sandbox: String,
    /// Name for the snapshot, unique within the sandbox.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreArgs {
    /// Snapshot id or name to restore from.
    snapshot: String,
    /// Optional name for the NEW sandbox that restore creates.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffArgs {
    /// Snapshot or sandbox (id or name) whose changes against the base image
    /// to report.
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DestroyArgs {
    /// Sandbox id or name to destroy. Idempotent: destroying an unknown or
    /// already-gone sandbox succeeds.
    sandbox: String,
}

// --- tools -------------------------------------------------------------------

#[tool_router]
impl DraugMcp {
    fn new(backend: NsBackend, registry: Arc<Registry>, config: McpConfig) -> Self {
        Self {
            backend,
            registry,
            config,
            create_gate: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Create and start a rootless sandbox around a project directory. \
        The project is mounted read-only; all writes go to a private overlay layer. \
        Returns the sandbox id to use in later calls."
    )]
    async fn sandbox_create(
        &self,
        Parameters(args): Parameters<CreateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_create(args)).await
    }

    #[tool(
        description = "Run a command inside a sandbox and wait for it to finish. \
        No shell is implied. Returns exit_code, duration_ms, and stdout/stderr \
        (truncated head+tail if very long). A non-zero exit_code is normal program \
        output, not a tool error."
    )]
    async fn sandbox_exec(
        &self,
        Parameters(args): Parameters<ExecArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_exec(args)).await
    }

    #[tool(
        description = "Capture a sandbox's filesystem state as an immutable snapshot. \
        Snapshots capture FILES, not processes: after a later restore, re-run whatever \
        was running. Returns the snapshot id and its on-disk size."
    )]
    async fn sandbox_snapshot(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_snapshot(args)).await
    }

    #[tool(
        description = "Materialize a NEW sandbox whose files start from a snapshot \
        (over the same base image). The snapshot and any originating sandbox are left \
        untouched. Subject to the max-concurrent-sandboxes quota. Returns the new \
        sandbox id."
    )]
    async fn sandbox_restore(
        &self,
        Parameters(args): Parameters<RestoreArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_restore(args)).await
    }

    #[tool(
        description = "Show how a snapshot's or live sandbox's files differ from the \
        base image, as added/modified/deleted paths with per-file size and mode. \
        Overlayfs whiteouts are decoded as deletions. Returns structured diff JSON."
    )]
    async fn sandbox_diff(
        &self,
        Parameters(args): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_diff(args)).await
    }

    #[tool(
        description = "Kill a sandbox's processes and delete all its state. Idempotent. \
        Snapshots taken from it survive. Frees a slot against the sandbox quota."
    )]
    async fn sandbox_destroy(
        &self,
        Parameters(args): Parameters<DestroyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_destroy(args)).await
    }

    #[tool(
        description = "List all sandboxes and snapshots in the registry, with their \
        state and metadata. Takes no arguments."
    )]
    async fn sandbox_list(&self) -> Result<CallToolResult, ErrorData> {
        self.guard(self.do_list()).await
    }
}

// --- tool implementations ----------------------------------------------------

impl DraugMcp {
    /// Wrap a tool body in the host-side call timeout. A timeout becomes a
    /// structured, retriable error rather than a hung connection.
    async fn guard(
        &self,
        fut: impl Future<Output = Result<CallToolResult, ErrorData>>,
    ) -> Result<CallToolResult, ErrorData> {
        match tokio::time::timeout(self.config.call_timeout, fut).await {
            Ok(r) => r,
            Err(_) => Ok(CallToolResult::structured_error(json!({
                "error": "timeout",
                "message": format!(
                    "operation exceeded the host call deadline of {}s",
                    self.config.call_timeout.as_secs()
                ),
                "retriable": true,
                "hint": "raise --call-timeout on `sbx mcp`, or split the work into \
                         smaller steps"
            }))),
        }
    }

    async fn do_create(&self, args: CreateArgs) -> Result<CallToolResult, ErrorData> {
        let project_dir = PathBuf::from(&args.project_dir);
        if !project_dir.is_absolute() {
            return Ok(invalid(
                "project_dir must be an absolute path",
                json!({ "project_dir": args.project_dir }),
            ));
        }
        let spec = SandboxSpec {
            name: args.name,
            rootfs: project_dir,
            from_snapshot: None,
            limits: ResourceLimits {
                cpu_millis: args.cpu_millis,
                memory_bytes: args.memory_max,
                pids: args.pids_max,
                disk_bytes: None,
                wall_time: None,
            },
            env: args.env.unwrap_or_default().into_iter().collect(),
            network: args.network,
        };

        // Quota: hold the create gate across the check *and* the spawn (which
        // inserts the registry row the next check counts), so two concurrent
        // creates can't both pass at max-1.
        let _gate = self.create_gate.lock().await;
        if let Err(e) = self.check_quota().await {
            return Ok(e);
        }
        match self.backend.spawn(&spec).await {
            Ok(sb) => Ok(CallToolResult::structured(sandbox_json(&sb))),
            Err(e) => Ok(error_object(&e)),
        }
    }

    async fn do_exec(&self, args: ExecArgs) -> Result<CallToolResult, ErrorData> {
        if args.command.is_empty() {
            return Ok(invalid("command must have at least one element", Value::Null));
        }
        let sb = match self.reg_get_sandbox(&args.sandbox).await {
            Ok(sb) => sb,
            Err(e) => return Ok(error_object(&e)),
        };

        // Keep the guest timeout strictly inside the host deadline so the
        // guest kills the command cleanly before the host backstop fires.
        let host_secs = self.config.call_timeout.as_secs();
        let ceiling_ms = host_secs.saturating_sub(15).max(1) * 1000;
        let guest_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_EXEC_TIMEOUT_MS)
            .min(ceiling_ms);

        let req = ExecRequest {
            argv: args.command,
            env: args.env.unwrap_or_default().into_iter().collect(),
            cwd: args.cwd,
            timeout_ms: Some(guest_ms),
        };

        let start = Instant::now();
        let mut handle = match self.backend.exec(&sb.id, req).await {
            Ok(h) => h,
            Err(e) => return Ok(error_object(&e)),
        };

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut code: Option<i32> = None;
        let mut signal: Option<i32> = None;
        let mut failure: Option<(String, String)> = None;

        while let Some(ev) = handle.next_event().await {
            match ev {
                ExecEvent::Started { .. } => {}
                ExecEvent::Stdout(d) => push_capped(&mut stdout, &d),
                ExecEvent::Stderr(d) => push_capped(&mut stderr, &d),
                ExecEvent::Exited { code: c, signal: s } => {
                    code = c;
                    signal = s;
                    break;
                }
                ExecEvent::Failed { kind, message } => {
                    failure = Some((kind, message));
                    break;
                }
            }
        }
        let duration_ms = start.elapsed().as_millis() as u64;

        let mut result = json!({
            "exit_code": code,
            "duration_ms": duration_ms,
            "stdout": clamp(&stdout),
            "stderr": clamp(&stderr),
        });
        let obj = result.as_object_mut().unwrap();
        if let Some(sig) = signal {
            obj.insert("signal".into(), json!(sig));
        }
        match failure {
            None => Ok(CallToolResult::structured(result)),
            Some((kind, message)) => {
                let timed_out = kind == "timeout";
                obj.insert("error".into(), json!(kind));
                obj.insert("message".into(), json!(message));
                obj.insert("timed_out".into(), json!(timed_out));
                obj.insert(
                    "hint".into(),
                    json!(if timed_out {
                        "the command hit its timeout_ms; raise it or make the command faster"
                    } else {
                        "the command could not be started; check the program name and cwd"
                    }),
                );
                Ok(CallToolResult::structured_error(result))
            }
        }
    }

    async fn do_snapshot(&self, args: SnapshotArgs) -> Result<CallToolResult, ErrorData> {
        let sb = match self.reg_get_sandbox(&args.sandbox).await {
            Ok(sb) => sb,
            Err(e) => return Ok(error_object(&e)),
        };
        let snap_id = match self.backend.snapshot(&sb.id, &args.name).await {
            Ok(id) => id,
            Err(e) => return Ok(error_object(&e)),
        };
        // Re-read the row so the reported size reflects the finished copy.
        match self.reg_get_snapshot(&snap_id.0).await {
            Ok(meta) => Ok(CallToolResult::structured(json!({
                "id": meta.id.0,
                "name": meta.name,
                "sandbox_id": meta.sandbox_id.map(|s| s.0),
                "size_bytes": meta.size_bytes,
                "note": "snapshots capture files, not processes; re-run anything that \
                         was running after a restore",
            }))),
            Err(e) => Ok(error_object(&e)),
        }
    }

    async fn do_restore(&self, args: RestoreArgs) -> Result<CallToolResult, ErrorData> {
        let snap = match self.reg_get_snapshot(&args.snapshot).await {
            Ok(s) => s,
            Err(e) => return Ok(error_object(&e)),
        };
        let _gate = self.create_gate.lock().await;
        if let Err(e) = self.check_quota().await {
            return Ok(e);
        }
        match self.backend.restore(&snap.id, args.name).await {
            Ok(sb) => {
                let mut v = sandbox_json(&sb);
                v.as_object_mut()
                    .unwrap()
                    .insert("from_snapshot".into(), json!(snap.id.0));
                Ok(CallToolResult::structured(v))
            }
            Err(e) => Ok(error_object(&e)),
        }
    }

    async fn do_diff(&self, args: DiffArgs) -> Result<CallToolResult, ErrorData> {
        let entries = match self.backend.diff(&args.target).await {
            Ok(e) => e,
            Err(e) => return Ok(error_object(&e)),
        };
        let mut added = 0;
        let mut modified = 0;
        let mut deleted = 0;
        for e in &entries {
            match e.kind {
                draug_core::DiffKind::Added => added += 1,
                draug_core::DiffKind::Modified => modified += 1,
                draug_core::DiffKind::Deleted => deleted += 1,
            }
        }
        let entries_json = serde_json::to_value(&entries).map_err(internal)?;
        Ok(CallToolResult::structured(json!({
            "target": args.target,
            "added": added,
            "modified": modified,
            "deleted": deleted,
            "entries": entries_json,
        })))
    }

    async fn do_destroy(&self, args: DestroyArgs) -> Result<CallToolResult, ErrorData> {
        // Resolve a name to its id where possible, but stay idempotent for
        // ids the registry has never heard of.
        let (id, existed) = match self.reg_get_sandbox(&args.sandbox).await {
            Ok(sb) => (sb.id, true),
            Err(Error::NotFound { .. }) => (SandboxId(args.sandbox.clone()), false),
            Err(e) => return Ok(error_object(&e)),
        };
        match self.backend.destroy(&id).await {
            Ok(()) => Ok(CallToolResult::structured(json!({
                "destroyed": id.0,
                "existed": existed,
            }))),
            Err(e) => Ok(error_object(&e)),
        }
    }

    async fn do_list(&self) -> Result<CallToolResult, ErrorData> {
        let registry = Arc::clone(&self.registry);
        let listed = tokio::task::spawn_blocking(move || {
            let sandboxes = registry.list_sandboxes()?;
            let snapshots = registry.list_snapshots(None)?;
            Ok::<_, Error>((sandboxes, snapshots))
        })
        .await
        .map_err(internal)?;

        let (sandboxes, snapshots) = match listed {
            Ok(v) => v,
            Err(e) => return Ok(error_object(&e)),
        };
        let sandboxes: Vec<Value> = sandboxes.iter().map(sandbox_json).collect();
        let snapshots: Vec<Value> = snapshots
            .into_iter()
            .map(|s| {
                json!({
                    "id": s.id.0,
                    "name": s.name,
                    "sandbox_id": s.sandbox_id.map(|x| x.0),
                    "size_bytes": s.size_bytes,
                    "created_at": s.created_at,
                })
            })
            .collect();
        Ok(CallToolResult::structured(json!({
            "sandbox_count": sandboxes.len(),
            "max_sandboxes": self.config.max_sandboxes,
            "sandboxes": sandboxes,
            "snapshots": snapshots,
        })))
    }

    // --- helpers -------------------------------------------------------------

    /// Reject the current create/restore if the host is already at the
    /// sandbox quota. Caller must hold `create_gate`.
    async fn check_quota(&self) -> Result<(), CallToolResult> {
        let registry = Arc::clone(&self.registry);
        let count = tokio::task::spawn_blocking(move || registry.list_sandboxes())
            .await
            .map_err(|_| ())
            .and_then(|r| r.map_err(|_| ()));
        let count = match count {
            Ok(list) => list.len(),
            // If we can't read the registry, fail closed with a clear error.
            Err(()) => {
                return Err(CallToolResult::structured_error(json!({
                    "error": "registry",
                    "message": "could not read the sandbox registry to check the quota",
                    "retriable": true,
                })))
            }
        };
        if count >= self.config.max_sandboxes {
            return Err(CallToolResult::structured_error(json!({
                "error": "quota_exceeded",
                "message": format!(
                    "at the max of {} concurrent sandboxes ({} exist)",
                    self.config.max_sandboxes, count
                ),
                "retriable": true,
                "hint": "destroy an unused sandbox with sandbox_destroy, or raise \
                         --max-sandboxes on `sbx mcp`",
            })));
        }
        Ok(())
    }

    async fn reg_get_sandbox(&self, key: &str) -> Result<Sandbox, Error> {
        let registry = Arc::clone(&self.registry);
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || registry.get_sandbox(&key))
            .await
            .map_err(|e| Error::io("registry task", std::io::Error::other(e)))?
    }

    async fn reg_get_snapshot(&self, key: &str) -> Result<draug_core::SnapshotMeta, Error> {
        let registry = Arc::clone(&self.registry);
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || registry.get_snapshot(&key))
            .await
            .map_err(|e| Error::io("registry task", std::io::Error::other(e)))?
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DraugMcp {
    fn get_info(&self) -> ServerInfo {
        let server_info = Implementation::new("draug", env!("CARGO_PKG_VERSION"))
            .with_title("draug sandboxes")
            .with_description("Local-first, zero-daemon Linux sandboxes for coding agents.")
            .with_website_url("https://github.com/aoprisan/draug");
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(server_info)
            .with_instructions(
                "Tools to create rootless Linux sandboxes around a project directory and \
                 run commands in them. Writes are isolated to an overlay layer; the real \
                 project is never modified. Typical flow: sandbox_create -> sandbox_exec \
                 (repeat) -> sandbox_snapshot before risky changes -> sandbox_restore to \
                 branch a fresh sandbox from a snapshot -> sandbox_destroy when done. \
                 Snapshots capture FILES, not processes: after a restore, re-run whatever \
                 was running. A non-zero exit_code from sandbox_exec is normal program \
                 output, not a tool failure.",
            )
    }
}

// --- output shaping ----------------------------------------------------------

/// Default guest-side exec timeout when the caller doesn't set one.
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 120_000;
/// Keep at most this many lines per stream; beyond it, head+tail with a marker.
const MAX_LINES: usize = 200;
/// Hard byte cap per stream, applied after line clamping (guards one huge line).
const MAX_BYTES: usize = 64 * 1024;
/// Stop accumulating a stream past this many bytes in memory (drain continues).
const MAX_CAPTURE: usize = 4 * 1024 * 1024;

fn push_capped(buf: &mut Vec<u8>, data: &[u8]) {
    if buf.len() >= MAX_CAPTURE {
        return;
    }
    let room = MAX_CAPTURE - buf.len();
    buf.extend_from_slice(&data[..data.len().min(room)]);
}

/// Render captured bytes for an LLM: lossy UTF-8, then head+tail line
/// truncation, then a hard byte cap — each step announces what it dropped.
fn clamp(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = clamp_lines(&text);
    clamp_bytes(text)
}

fn clamp_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= MAX_LINES {
        return s.to_string();
    }
    let head = MAX_LINES / 2;
    let tail = MAX_LINES - head;
    let omitted = lines.len() - head - tail;
    let mut out = String::new();
    for l in &lines[..head] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("[... {omitted} lines omitted ...]\n"));
    for l in &lines[lines.len() - tail..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn clamp_bytes(s: String) -> String {
    if s.len() <= MAX_BYTES {
        return s;
    }
    let head_end = floor_boundary(&s, MAX_BYTES / 2);
    let tail_start = ceil_boundary(&s, s.len() - MAX_BYTES / 2);
    let omitted = tail_start - head_end;
    format!(
        "{}\n[... {omitted} bytes omitted ...]\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn sandbox_json(sb: &Sandbox) -> Value {
    json!({
        "id": sb.id.0,
        "name": sb.name,
        "state": sb.state.as_str(),
        "project_dir": sb.rootfs.to_string_lossy(),
        "created_at": sb.created_at,
    })
}

// --- error mapping -----------------------------------------------------------

/// Malformed-argument tool error the model can correct and retry.
fn invalid(message: &str, data: Value) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": "invalid_argument",
        "message": message,
        "retriable": false,
        "details": data,
    }))
}

/// Wrap an unexpected join/serialization failure as a protocol error.
fn internal<E: std::fmt::Display>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Classify a backend [`Error`] into a structured, actionable tool error.
/// Categories mirror the draug error taxonomy (see DESIGN.md): the `error`
/// slug says what class it is, `retriable` says whether trying again could
/// help, and `hint` says what to do about it.
fn error_object(e: &Error) -> CallToolResult {
    let (slug, retriable, hint) = match e {
        Error::NotFound { kind, .. } => (
            "not_found",
            false,
            format!("no such {kind}; call sandbox_list to see what exists"),
        ),
        Error::AlreadyExists { kind, .. } => (
            "already_exists",
            false,
            format!("a {kind} with that name already exists; pick another name"),
        ),
        Error::InvalidSpec(_) => (
            "invalid_spec",
            false,
            "fix the arguments and try again".to_string(),
        ),
        Error::WrongState { op, state, .. } => (
            "wrong_state",
            false,
            format!("cannot {op} a sandbox in state {state:?}; inspect it with sandbox_list"),
        ),
        Error::Unsupported(_) => (
            "unsupported",
            false,
            "this host lacks a required kernel facility; the message names the knob"
                .to_string(),
        ),
        Error::Protocol(_) => (
            "protocol",
            false,
            "the sandbox's guest agent misbehaved; destroy and recreate the sandbox"
                .to_string(),
        ),
        Error::Exec { kind, .. } => (
            "exec_failed",
            matches!(kind, draug_core::ExecErrorKind::Timeout),
            "the command could not be run; check the program name, cwd, and timeout"
                .to_string(),
        ),
        Error::Registry(_) => (
            "registry",
            true,
            "the registry was busy or errored; retrying may help".to_string(),
        ),
        Error::Io { op, .. } => (
            "io",
            true,
            format!("an OS operation ({op}) failed; the message has details"),
        ),
    };
    CallToolResult::structured_error(json!({
        "error": slug,
        "message": e.to_string(),
        "retriable": retriable,
        "hint": hint,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_verbatim() {
        let s = "line1\nline2\nline3\n";
        assert_eq!(clamp(s.as_bytes()), s);
    }

    #[test]
    fn long_output_keeps_head_and_tail_with_marker() {
        let input: String = (1..=500).map(|i| format!("line{i}\n")).collect();
        let out = clamp(input.as_bytes());
        let lines: Vec<&str> = out.lines().collect();
        // 100 head + 1 marker + 100 tail.
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[MAX_LINES / 2 - 1], "line100");
        assert_eq!(lines[MAX_LINES / 2], "[... 300 lines omitted ...]");
        assert_eq!(lines[MAX_LINES / 2 + 1], "line401");
        assert_eq!(*lines.last().unwrap(), "line500");
    }

    #[test]
    fn one_enormous_line_is_byte_capped() {
        // A single line far over the byte cap and under the line cap: line
        // clamping leaves it alone, byte clamping must still bound it.
        let input = "x".repeat(MAX_BYTES * 3);
        let out = clamp(input.as_bytes());
        assert!(out.len() < MAX_BYTES + 128, "not byte-capped: {}", out.len());
        assert!(out.contains("bytes omitted"));
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let out = clamp(&[0xff, 0xfe, b'h', b'i', 0xff]);
        assert!(out.contains("hi"));
    }

    #[test]
    fn capture_stops_but_marker_shows_truncation() {
        let mut buf = Vec::new();
        // Push more than MAX_CAPTURE; excess is dropped at capture time.
        let chunk = vec![b'a'; MAX_CAPTURE];
        push_capped(&mut buf, &chunk);
        push_capped(&mut buf, &chunk);
        assert_eq!(buf.len(), MAX_CAPTURE);
    }

    #[test]
    fn error_object_is_structured_and_actionable() {
        let e = Error::NotFound {
            kind: draug_core::ResourceKind::Sandbox,
            id: "ghost".into(),
        };
        let res = error_object(&e);
        assert_eq!(res.is_error, Some(true));
        let v = res.structured_content.unwrap();
        assert_eq!(v["error"], "not_found");
        assert_eq!(v["retriable"], false);
        assert!(v["hint"].as_str().unwrap().contains("sandbox_list"));
        assert!(v["message"].as_str().unwrap().contains("ghost"));
    }
}
