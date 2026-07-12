//! draug-guest: PID 1 inside a draug sandbox.
//!
//! Will: listen on the guest unix socket, speak the length-prefixed msgpack
//! exec protocol (see DESIGN.md), reap zombies, and enforce exec timeouts.
//!
//! Currently a stub.

fn main() {
    eprintln!("draug-guest: stub — exec protocol not implemented yet");
    std::process::exit(1);
}
