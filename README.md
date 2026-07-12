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

- Snapshots (`sbx snapshot` / `restore` / `diff`) and the MCP server
  (`sbx mcp`) are not implemented yet. When they land, snapshots capture
  **files, not processes** — see DESIGN.md.
- If the state directory itself sits on overlayfs (common in CI containers),
  the upper layer transparently falls back to a tmpfs inside the sandbox’s
  mount namespace: everything works, but project changes die with the
  sandbox. A warning is printed.
- Hosts that mask `/proc` (hardened container runtimes) can't get a private
  `/proc`; the sandbox then sees the host's, with a warning.
- The exec protocol supports stdin frames, but `sbx exec` doesn't wire the
  terminal's stdin up yet, and there is no pty support.

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
