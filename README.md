# draug

A local-first, zero-daemon sandbox for AI coding agents, exposed as an MCP
server. Linux-only.

`sbx run` wraps a project directory in a rootless Linux sandbox: processes
inside see the host's toolchains read-only, see the project read-write, and
everything they write to the project lands in an overlay upper layer on the
host — never in the real project directory. No daemon, no root: each sandbox
is kept alive by its own PID 1, and a SQLite registry lets later invocations
find it.

```console
$ sbx run ~/code/myapp --name dev
sandbox 3fa1c2d9 (dev) is ready
$ sbx exec dev -- cargo test          # streams output, returns the real exit code
$ sbx exec dev -- sh -c 'echo hi > NEW'
$ ls ~/code/myapp/NEW                 # the real project is untouched
ls: cannot access '.../NEW': No such file or directory
$ sbx destroy dev
```

## What a sandbox is

- **User + mount + pid + net + uts namespaces**, created rootless via
  `newuidmap`/`newgidmap` (sandbox root maps to you; other uids map to your
  subordinate range from `/etc/subuid`).
- **Filesystem**: system directories (`/usr`, `/etc`, `/opt`, ...) are
  bind-mounted read-only; `/home` and other user-writable host paths are
  simply absent. The project directory is an **overlayfs** mount — read-only
  lower layer = your real project, writes go to a per-sandbox upper layer
  under `$XDG_STATE_HOME/draug`.
- **Network**: none by default — an isolated netns with only loopback.
  `--network` opts back into the host network.
- **Resource limits**: `--memory-max`, `--cpu-millis`, `--pids-max` via
  cgroup v2, when a delegated subtree is available.
- **Exec**: `sbx exec` talks to the sandbox's PID 1 over a unix socket
  (length-prefixed msgpack), streams stdout/stderr incrementally, passes the
  exit code through, and can enforce a `--timeout` inside the sandbox.

See [DESIGN.md](DESIGN.md) for the trait contracts, exec protocol, and
snapshot semantics.

## Requirements

- **Linux, kernel ≥ 5.11.** Rootless operation needs unprivileged overlayfs
  mounts inside a user namespace with the `userxattr` option, which landed in
  5.11. Older kernels fail at the overlay mount with an actionable error.
- **Unprivileged user namespaces enabled** (`sysctl user.max_user_namespaces`
  > 0; on Debian-family kernels also `kernel.unprivileged_userns_clone=1`).
- **`newuidmap`/`newgidmap`** (package `uidmap` on Debian/Ubuntu,
  `shadow-utils` on Fedora) and a subordinate id range in `/etc/subuid` and
  `/etc/subgid`, e.g. `you:100000:65536`. Missing configuration produces an
  error that says exactly what to add. (Running as real root skips this.)
- For resource limits only: a **delegated cgroup v2 subtree** (default on
  systemd desktops; otherwise `systemd-run --user --scope -p Delegate=yes`).
  Sandboxes without limits don't touch cgroups at all.

## Honest limitations (current state)

- Snapshots capture **files, not processes**: after `sbx restore` (or the
  `sandbox_restore` MCP tool) the files are exactly as captured, but nothing
  is running — re-run whatever was running. See DESIGN.md.
- If the state directory itself sits on overlayfs (common in CI containers),
  the upper layer transparently falls back to a tmpfs inside the sandbox’s
  mount namespace: everything works, but project changes die with the
  sandbox. A warning is printed.
- Hosts that mask `/proc` (hardened container runtimes) can't get a private
  `/proc`. draug **fails closed** here rather than expose the host's `/proc`
  (which would leak host processes and the user's files via
  `/proc/<pid>/root`). Pass `--insecure-host-proc` to override on a trusted
  host — it bind-mounts the host `/proc` and prints a loud warning.
- The exec protocol supports stdin frames, but `sbx exec` doesn't wire the
  terminal's stdin up yet, and there is no pty support.

## Use as an MCP server

`sbx mcp` exposes the sandbox toolset to an LLM agent as an [MCP](https://modelcontextprotocol.io)
server speaking JSON-RPC 2.0 over stdio (built on the official Rust SDK,
[`rmcp`](https://crates.io/crates/rmcp)). The agent gets seven tools:

| Tool | What it does |
|---|---|
| `sandbox_create` | Start a sandbox around a project directory |
| `sandbox_exec` | Run a command; returns `{ exit_code, duration_ms, stdout, stderr }` |
| `sandbox_snapshot` | Capture the sandbox's files as an immutable snapshot |
| `sandbox_restore` | Materialize a **new** sandbox from a snapshot |
| `sandbox_diff` | Structured added/modified/deleted paths vs. the base image |
| `sandbox_destroy` | Kill and delete a sandbox (idempotent) |
| `sandbox_list` | List all sandboxes and snapshots |

Results are shaped for a model, not a terminal: every tool returns structured
JSON, `sandbox_exec` truncates long `stdout`/`stderr` to head+tail with a
`[... N lines omitted ...]` marker (~200 lines each), and every failure is a
structured `{ error, message, retriable, hint }` object rather than a panic or
an opaque protocol error.

Two guards bound a runaway agent, both configurable:

- `--call-timeout <seconds>` — host-side deadline on every tool call
  (default 300). For `sandbox_exec` the in-sandbox `timeout_ms` is clamped to
  stay inside this, so the command is killed cleanly before the backstop.
- `--max-sandboxes <n>` — refuse `sandbox_create` / `sandbox_restore` past
  this many live sandboxes (default 8), returning a `quota_exceeded` error.

### Register in Claude Code

The fastest way is the CLI, which writes the config for you:

```console
$ claude mcp add draug -- sbx mcp
```

Or add it by hand to a `.mcp.json` at your project root (checked in, shared
with your team) or to `~/.claude.json` (personal). The server is a plain
stdio subprocess — `command` + `args`, no ports:

```json
{
  "mcpServers": {
    "draug": {
      "command": "sbx",
      "args": ["mcp", "--max-sandboxes", "8", "--call-timeout", "300"]
    }
  }
}
```

Use an absolute `command` (e.g. `/usr/local/bin/sbx`, or the built
`target/release/sbx`) if `sbx` isn't on the PATH that Claude Code launches
with. The registry and sandbox state live under `$XDG_DATA_HOME`/
`$XDG_STATE_HOME` as usual, so sandboxes created over MCP are the same ones
`sbx` sees on the command line — `sandbox_list` enumerates them, and you can
clean any of them up with `sbx destroy <id>`. Diagnostics go to stderr;
stdout is reserved for the JSON-RPC stream.

## Workspace layout

| Crate | Role |
|---|---|
| `draug-core` | `Backend` trait, SQLite registry, limits, error taxonomy |
| `draug-proto` | Exec protocol wire types + framing (tokio-free) |
| `draug-ns` | The rootless namespace backend described above |
| `draug-guest` | Guest agent (PID 1 inside the sandbox) |
| `draug-cli` | The `sbx` binary |

## Development

```console
$ cargo test --workspace
```

The integration tests create real sandboxes and skip themselves (with the
reason) on non-Linux hosts or when user namespaces are unavailable.
