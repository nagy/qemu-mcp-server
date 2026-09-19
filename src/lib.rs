//! Library shim exposing the server's public API for doctests.
//!
//! `cargo test --doc` needs a library target; a binary-only crate is
//! skipped entirely. This shim includes the server's single source file
//! as a private module and re-exports its public items so doctests can
//! compile against them.

// `main` and its private helpers are used only by the binary target;
// from the library's public API they look dead. Silence the false
// positives here instead of scattering allows through the source.
#[allow(dead_code)]
#[path = "../qemu_mcp_server.rs"]
mod imp;

pub use imp::{QMPSocket, QmpRequest, SerialReadRequest, SerialWriteRequest};
