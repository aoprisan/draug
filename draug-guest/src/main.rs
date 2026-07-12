//! draug-guest binary: standalone entry for the guest agent.
//!
//! The namespace backend doesn't exec this binary — it forks straight into
//! `draug_guest::guest_main` after setup. This entry point exists for the
//! future static-musl guest used by VM-class backends: it takes the
//! GuestConfig as a JSON argument.

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: draug-guest <guest-config-json>");
        std::process::exit(2);
    });
    let cfg: draug_guest::GuestConfig = serde_json::from_str(&arg).unwrap_or_else(|e| {
        eprintln!("draug-guest: bad config: {e}");
        std::process::exit(2);
    });
    draug_guest::guest_main(cfg)
}
