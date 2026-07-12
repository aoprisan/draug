//! Exec protocol types: length-prefixed msgpack over a unix socket.
//!
//! The wire types live in the tokio-free `draug-proto` crate so the static
//! guest binary can link them without the async runtime; this module
//! re-exports them for host-side code. See DESIGN.md for the protocol.

pub use draug_proto::*;
