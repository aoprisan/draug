//! sbx: the draug CLI. All subcommands are stubs for now.

use clap::{Parser, Subcommand};

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
    /// Create and start a sandbox from a base image
    Run {
        /// Read-only base image directory (overlayfs lowerdir)
        rootfs: String,
        /// Human-readable sandbox name
        #[arg(long)]
        name: Option<String>,
        /// Seed the writable layer from an existing snapshot
        #[arg(long)]
        from_snapshot: Option<String>,
    },
    /// Run a command inside a sandbox, streaming its output
    Exec {
        /// Sandbox id or name
        sandbox: String,
        /// Command and arguments (no shell is implied)
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Capture a sandbox's filesystem state.
    /// Snapshots capture files, not processes: after restore, re-run
    /// whatever was running.
    Snapshot {
        /// Sandbox id or name
        sandbox: String,
        /// Snapshot name
        name: String,
    },
    /// Replace a sandbox's filesystem with a snapshot's contents
    Restore {
        /// Sandbox id or name
        sandbox: String,
        /// Snapshot id or name
        snapshot: String,
    },
    /// Show filesystem changes between two snapshots, or between a
    /// snapshot and the live sandbox
    Diff {
        /// Snapshot id or name (base)
        from: String,
        /// Snapshot id or name, or sandbox id (target); defaults to the
        /// snapshot's live sandbox
        to: Option<String>,
    },
    /// Kill a sandbox's processes and delete all its state
    Destroy {
        /// Sandbox id or name
        sandbox: String,
    },
    /// Serve the draug MCP server over stdio
    Mcp,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let what = match cli.command {
        Command::Run { .. } => "run",
        Command::Exec { .. } => "exec",
        Command::Snapshot { .. } => "snapshot",
        Command::Restore { .. } => "restore",
        Command::Diff { .. } => "diff",
        Command::Destroy { .. } => "destroy",
        Command::Mcp => "mcp",
    };
    eprintln!("sbx {what}: not implemented yet");
    std::process::exit(1);
}
