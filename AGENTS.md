# AGENTS.md — qemu-mcp-server

## Purpose

`qemu-mcp-server`: Rust binary that exposes QEMU instances over the Model Context
Protocol (MCP) through QMP. `src/main.rs` is a thin wrapper around
`qemu_mcp_server::run()`.

## Layout

- `src/lib.rs` — crate docs, `QMPSocket`, the `#[tool_router]` tool methods,
  `ServerHandler`, `run()`, and the request structs.
- `src/qmp.rs` — `QmpError` and the QMP client helpers.
- `src/serial.rs` — `SerialError`, `SerialState`, and the buffered serial
  helpers.
- `src/path.rs` — `SocketPath`.
- `src/test_support.rs` — socket fixtures shared by the test modules. Sits
  behind `#[cfg(test)]`, so it never enters the binary.
- `flake.nix` — package, checks, devShell, treefmt.

## Build and test

- `nix flake check` — the gate. Run before done.
- `nix build` — build the binary into `./result`.
- `nix fmt` — format flake, Rust and TOML through treefmt. Never run formatters
  standalone.
- `nix build .#qemu-mcp-server-doc` — static rustdoc HTML in `result/share/doc`.
- `nix develop` — dev shell with cargo.

## Architecture

- `lib.rs` serves as a library target, not only a test home. An external caller
  builds a server with `QMPSocket::with_paths` (explicit socket paths) or
  `QMPSocket::new` (paths from the environment), then serves it on any rmcp
  transport. `run()` is the stdio convenience entry point.
- Socket paths cross the API as `SocketPath` values, so the QMP path and the
  serial path cannot swap places at a call site.
- Failures cross the API as `QmpError` and `SerialError`. Both convert into
  rmcp's `McpError` at the tool boundary.
- Each module holds its own `#[cfg(test)] mod tests` next to the code it covers.
  `lib.rs` carries no unit tests. The request structs have doctests only.
- Every module file carries one `//!` doc. The crate root declares the modules
  bare and re-exports the public types.

## Rules

- Lint levels live in the `[lints]` table in `Cargo.toml`, never on a command
  line. `all` sits at `deny`, `pedantic` and `nursery` at `warn`. Silence a lint
  at its site with `#[expect(..., reason = "...")]`. The crate holds no
  `clippy.toml`.

## Conventions

- Rust edition 2024. The rustfmt config lives in `flake.nix` under treefmt.
- Derive `Display`, `Error`, `From` and `AsRef` with `derive_more` (features
  `display`, `error`, `from`, `as_ref`). Hand-written impls serve as the
  fallback.
