# draug — design

A local-first, zero-daemon sandbox for AI coding agents, exposed as an MCP
server. Linux-only.

**Local-first, zero-daemon** means: there is no long-running privileged
service. Every sandbox is owned by the process that spawned it (the `sbx` CLI
or the MCP server it runs). Durable state — which sandboxes exist, where their
filesystems live, what snapshots they have — is recorded in a per-user SQLite
database so a later invocation can reattach, inspect, or destroy them. The
database is the source of truth for *metadata*; the kernel (mounts, processes,
cgroups) is the source of truth for *liveness*, and the registry reconciles
against it rather than trusting itself.

## Crate layout

| Crate | Role |
|---|---|
| `draug-core` | `Backend` trait, sandbox/snapshot registry (SQLite via `rusqlite`), resource-limit types, error taxonomy |
| `draug-proto` | Exec protocol wire types + sync framing; tokio-free so the guest can link it |
| `draug-ns` | Namespace backend: user/mount/pid/net/uts namespaces + overlayfs + cgroup v2. The default (and currently only) backend. |
| `draug-guest` | Guest agent library + binary. PID 1 inside the sandbox; speaks the exec protocol over a unix socket. |
| `draug-cli` | The `sbx` binary (clap): `run`, `exec`, `snapshot`, `restore`, `diff`, `destroy`, `mcp` |

Dependency direction: `draug-cli → draug-ns → {draug-core, draug-guest} →
draug-proto`. `draug-core::proto` re-exports `draug-proto`. In the namespace
backend the guest is not exec'd as a separate binary: the setup process
(a re-execution of the host binary itself, see draug-ns docs) forks and
calls `draug_guest::guest_main` directly, so any binary embedding draug-ns
must call `draug_ns::reexec::maybe_run()` first thing in `main`. The
standalone `draug-guest` binary (static musl) is the entry point reserved
for future VM-class backends.

## The `Backend` trait

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    /// Create and start a sandbox from a spec. Registers it in the registry
    /// before the guest is launched, so a crash mid-spawn leaves a row that
    /// `destroy` can clean up.
    async fn spawn(&self, spec: &SandboxSpec) -> Result<Sandbox>;

    /// Run a command inside a running sandbox. Returns a handle streaming
    /// stdout/stderr chunks and, finally, the exit status.
    async fn exec(&self, id: &SandboxId, req: ExecRequest) -> Result<ExecHandle>;

    /// Capture the sandbox's *filesystem* state (see "Snapshot semantics").
    async fn snapshot(&self, id: &SandboxId, name: &str) -> Result<SnapshotId>;

    /// Materialize a *new* sandbox whose writable layer starts from the
    /// snapshot's captured layer (over the same base image). The snapshot is
    /// immutable and restorable any number of times; the originating sandbox
    /// is untouched. Files come back exactly as captured; processes do not.
    async fn restore(&self, snapshot: &SnapshotId, name: Option<String>) -> Result<Sandbox>;

    /// Kill everything, unmount, delete on-disk state, remove from registry.
    /// Idempotent: destroying a half-created or already-gone sandbox is Ok.
    async fn destroy(&self, id: &SandboxId) -> Result<()>;
}
```

### Async semantics (tokio)

- All trait methods are `async` and run on the tokio multi-threaded runtime.
  The trait is object-safe (`dyn Backend`) via `async_trait`; when the MSRV
  story allows, this can migrate to native AFIT + `dyn`-dispatch glue.
- **No method blocks the executor.** Anything that can stall — `mount(2)`,
  `clone(2)`, cgroup writes, SQLite calls, directory copies — runs under
  `tokio::task::spawn_blocking` (or dedicated blocking threads for long
  copies). `rusqlite` is synchronous by design; the registry wraps a
  `Mutex<Connection>` and every registry call made from async context goes
  through `spawn_blocking`. Registry operations are single small
  transactions, so contention on that mutex is not a concern at this scale.
- **Cancellation safety:** dropping the future of `spawn`/`snapshot`/
  `restore` must not leak kernel state unaccounted for. The rule is
  *registry-first*: record intent (row in state `Creating`/`Restoring`)
  before touching the kernel, and mark `Ready` only after. Anything left in
  an intermediate state is garbage-collectable by `destroy` or a future
  `sbx gc`. Dropping an `ExecHandle` kills the remote command (best-effort
  SIGKILL to the exec'd process group via the guest).
- `Backend` implementations are cheap handles (`Arc` internals) — cloneable,
  shareable across tasks; per-sandbox mutation is serialized by a
  per-sandbox async lock inside the backend, so concurrent `exec` +
  `snapshot` on the same sandbox cannot interleave destructively.

### Sandbox lifecycle states

```
Creating → Ready ⇄ Running(exec in flight) → Stopped → Destroyed
     ↘ (crash/partial) any state → destroy() → Destroyed
```

States are stored in the registry. `Destroyed` rows are deleted, not kept
(history can come later via an events table).

## Snapshot semantics — filesystem-level, and honestly so

**A draug snapshot is a filesystem snapshot, not a process snapshot.**

The sandbox root is an overlayfs mount:

```
lowerdir = base image (read-only, shared across sandboxes)
upperdir = this sandbox's writes
workdir  = overlayfs bookkeeping
merged   = what the sandbox sees as /
```

`snapshot` captures the **upper layer only**: quiesce writers (the guest
briefly pauses spawned processes' filesystem work by freezing the cgroup),
`syncfs`, then copy (or reflink, on XFS/btrfs) the upperdir into
`snapshots/<id>/`. The base image is immutable and shared, so a snapshot is
just "the delta this sandbox has made", which is small and fast to copy.

`restore` spawns a *new* sandbox and seeds its upperdir from a copy of the
snapshot's captured layer, over the same base image. The new sandbox comes up
with the *files* exactly as they were at snapshot time; the snapshot stays
immutable and the sandbox it was taken from is left running/untouched.
(Seeding a fresh sandbox rather than mutating the original keeps snapshots
reusable and sidesteps the unmount/remount dance on a live overlay.)

What this deliberately does **not** capture, and users must not expect:

- **Process state.** Running processes, their memory, open file descriptors,
  PIDs, and signal state are gone after restore. A dev server that was
  running at snapshot time is *not running* after restore — the agent must
  start it again. (Full process checkpointing à la CRIU is explicitly out of
  scope: it is fragile, kernel-version-sensitive, and unnecessary for the
  agent use case, where "same files, fresh processes" is the useful
  contract.)
- **Kernel state**: network connections, System V IPC, timers, `/proc`
  contents, mounted tmpfs contents (tmpfs is not part of the overlay upper
  layer — anything in `/tmp` or `/dev/shm` is lost unless the spec maps them
  onto the overlay).
- **In-flight writes** not yet visible at the VFS layer — mitigated by the
  freeze + `syncfs` step, but a process killed mid-`write(2)` sequence sees
  torn *application-level* state exactly as it would after a power cut.

The one-line contract exposed to agents (and in `sbx snapshot --help`):
*"Snapshots capture files, not processes. After restore, re-run whatever was
running."*

Snapshots are immutable once taken, are metadata-registered in SQLite
(id, sandbox id, name, created-at, size, parent base image), and survive the
sandbox they came from — `spawn` may take `--from-snapshot` to seed a new
sandbox's upperdir (this is exactly what `restore` does). `diff` walks a
single layer — a snapshot's captured upper, or a live sandbox's upper — and
reports how it differs from its base image as added/modified/deleted paths:
overlayfs represents deletions as character-0:0 whiteout devices and replaced
directories via an `overlay.opaque` xattr, so the walker decodes those rather
than treating them as regular files. Output is structured (`DiffEntry` with
path, kind, and per-file size/mode), rendered as text or JSON by the CLI.

## Exec protocol

Transport: a unix stream socket per sandbox, host path
`<state-dir>/rt/guest.sock`, bind-mounted into the sandbox at `/run/draug`
where the guest agent (PID 1 inside) listens. The host
side connects per-exec; the guest multiplexes nothing — **one connection ==
one exec**, which keeps framing trivial and lets connection close double as
cancellation.

Framing: **length-prefixed msgpack**. Each frame is:

```
u32 big-endian length N  |  N bytes: one msgpack-encoded Message
```

Max frame size 1 MiB (guest rejects larger with a protocol error frame);
stdout/stderr chunks are cut to ≤64 KiB so a chatty command cannot starve
the stderr stream behind one giant stdout frame.

Messages (host→guest, then guest→host):

```
// host → guest, exactly one, first frame on the connection
ExecRequest {
    argv: Vec<String>,          // argv[0] is the program; no shell implied
    env: Vec<(String, String)>,
    cwd: Option<String>,
    stdin: bool,                // whether Stdin frames will follow
    tty: bool,                  // allocate pty instead of pipes (later)
    timeout_ms: Option<u64>,
}
// host → guest, zero or more, only if stdin: true
Stdin { data: Vec<u8> }         // empty data == EOF (half-close)

// guest → host, streamed in whatever order the pipes produce
Started { pid: u32 }
Stdout { data: Vec<u8> }
Stderr { data: Vec<u8> }
Exit   { code: Option<i32>, signal: Option<i32> }   // terminal frame
Error  { kind: String, message: String }            // terminal frame
```

Properties:

- **Streaming**: `Stdout`/`Stderr` frames are forwarded to the host as they
  are read from the pipes — no buffering of whole output. On the host,
  `ExecHandle` exposes them as an ordered stream of `ExecEvent`s
  (`tokio::sync::mpsc`); order between stdout and stderr is the order the
  guest observed, which is as good as pipes allow.
- **Termination**: exactly one terminal frame (`Exit` or `Error`) per
  connection, after which the guest closes the socket. Host closing the
  socket early ⇒ guest SIGKILLs the process group (cancellation).
- **Timeouts** are enforced guest-side (`timeout_ms` → SIGKILL + `Error
  {kind: "timeout"}`), so a wedged host can't leave runaway processes; the
  host may additionally impose its own deadline.
- msgpack via `rmp-serde`; message enum is `#[serde(tag = ...)]`-free —
  encoded as a 2-tuple `(discriminant: u8, payload)` for forward
  compatibility and small frames. Unknown discriminants are a protocol
  error, not a skip: both ends are shipped from one repo, version skew is a
  bug we want loud.

## Error taxonomy

One public error enum in `draug-core` (`thiserror`), shared by all crates.
Categories are chosen by *what the caller should do about it*:

```rust
pub enum Error {
    /// Sandbox or snapshot id doesn't exist. Caller bug or stale handle —
    /// don't retry.
    NotFound { kind: ResourceKind, id: String },

    /// Name/id collision on create. Pick a different name.
    AlreadyExists { kind: ResourceKind, id: String },

    /// Spec rejected before touching the kernel (bad limits, missing
    /// rootfs, invalid name). Fix the input.
    InvalidSpec(String),

    /// Operation not valid for the sandbox's current state
    /// (e.g. exec on a Stopped sandbox). Inspect state, then decide.
    WrongState { id: String, state: SandboxState, op: &'static str },

    /// Host lacks a kernel facility (no unprivileged userns, no overlayfs,
    /// no cgroup v2 delegation). Not retryable; message says which knob.
    Unsupported(String),

    /// The guest agent misbehaved: bad frame, oversized frame, unexpected
    /// terminal, connection reset mid-exec. Sandbox may be poisoned;
    /// recommended handling is destroy + respawn.
    Protocol(String),

    /// The command ran and the *protocol* worked, but exec-level failure
    /// (spawn failed inside guest, timeout). Carries the guest's report.
    Exec { kind: ExecErrorKind, message: String },

    /// Registry (SQLite) failure. Possibly transient (locked), possibly
    /// corruption; message + source distinguish.
    Registry(#[from] rusqlite::Error),

    /// Everything the OS said no to: mounts, clone, cgroups, file copies.
    /// Wraps io::Error with an operation label for diagnosability.
    Io { op: &'static str, #[source] source: std::io::Error },
}
```

Deliberate non-goals of the taxonomy: no blanket `Other(anyhow::Error)`
variant in the public API — internal helpers may use `anyhow`, but
everything crossing the `Backend` boundary is classified. A nonzero exit
code from an exec'd command is **not** an error — it's a normal `Exit`
event; only failures to *run* the command are `Error::Exec`.

CLI mapping: each variant maps to a stable process exit code and a
one-line `error:` message on stderr; `--json` emits the variant name +
fields for MCP/tooling use.

## Registry (SQLite)

Single file, `~/.local/share/draug/registry.db` (overridable), WAL mode,
`busy_timeout` set so two concurrent `sbx` invocations queue instead of
failing. Schema v1:

```sql
CREATE TABLE sandbox (
    id         TEXT PRIMARY KEY,   -- short random id, also the dir name
    name       TEXT UNIQUE,        -- optional human name
    state      TEXT NOT NULL,      -- Creating|Ready|Running|Stopped
    backend    TEXT NOT NULL,      -- "ns"
    rootfs     TEXT NOT NULL,      -- base image path
    state_dir  TEXT NOT NULL,      -- upper/work/merged/socket live here
    limits     TEXT NOT NULL,      -- ResourceLimits as JSON
    created_at INTEGER NOT NULL
);
CREATE TABLE snapshot (
    id         TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL REFERENCES sandbox(id) ON DELETE SET NULL,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (sandbox_id, name)
);
PRAGMA user_version = 1;  -- migrations gate on this
```

## Resource limits

`ResourceLimits` in `draug-core`; enforced by the backend via cgroup v2
(cpu.max, memory.max, pids.max) plus a project-quota or loopback-image cap
for disk. All optional; `None` = unlimited:

```rust
pub struct ResourceLimits {
    pub cpu_millis: Option<u32>,      // 1000 = one full core (cpu.max)
    pub memory_bytes: Option<u64>,    // memory.max
    pub pids: Option<u32>,            // pids.max
    pub disk_bytes: Option<u64>,      // upperdir quota
    pub wall_time: Option<Duration>,  // whole-sandbox deadline, host-enforced
}
```

## Out of scope (for now)

- Non-Linux hosts; VM/microVM backends (the `Backend` trait is the seam
  where a firecracker backend would plug in later).
- Process-state snapshots (CRIU) — see snapshot section.
- Network policy beyond on/off; image building/pulling; multi-user daemons.
- CI is deliberately deferred; `deny(warnings)` stays off until the
  scaffolding settles.
