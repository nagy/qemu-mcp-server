# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project

`qemu-mcp-server` — a Rust binary exposing QEMU instances over the Model
Context Protocol (MCP) via QMP. Implementation lives in `src/lib.rs`;
`src/main.rs` is a thin binary wrapper around `qemu_mcp_server::run()`.

`src/lib.rs` is a usable library target, not only a test home. External
callers build a server with `QMPSocket::with_paths` (explicit socket
paths) or `QMPSocket::new` (paths from environment), then serve it on
any rmcp transport. `run()` is the stdio convenience entry point.
Socket paths cross the API as `SocketPath` values, so the QMP path and
the serial path cannot swap places at a call site. Failures cross the
API as `QmpError` and `SerialError`; both convert into `rmcp`'s
`McpError` at the tool boundary.

## Build & test

Nix flake (no default.nix anymore):

```bash
nix flake check   # builds the package and runs cargo tests + doctests; also runs clippy and a treefmt check
nix build         # build the binary into ./result
nix fmt           # format flake, Rust, and TOML via treefmt (never run formatters standalone)
nix build .#qemu-mcp-server-doc  # static rustdoc HTML in result/share/doc
```

Dev shell provides cargo (`nix develop`).

## Layout

- `src/lib.rs` — crate docs, `QMPSocket`, the `#[tool_router]` tool
  methods, `ServerHandler`, `run()`, and the request structs.
- `src/qmp.rs` — `QmpError` and the QMP client helpers.
- `src/serial.rs` — `SerialError`, `SerialState`, and the buffered
  serial helpers.
- `src/path.rs` — `SocketPath`.
- `src/test_support.rs` — socket fixtures shared by the test modules.
  It sits behind `#[cfg(test)]`, so it never enters the binary.

Each module holds its own `#[cfg(test)] mod tests` next to the code it
covers. `lib.rs` carries no unit tests; the request structs have
doctests only.

Every module file carries one `//!` doc. The crate root declares the
modules bare and re-exports the public types.

## Conventions

- Rust edition 2024; rustfmt config lives in `flake.nix` (treefmt).
- Lint levels live in the `[lints]` table in `Cargo.toml`, never on a
  command line. `all` sits at `deny`, `pedantic` and `nursery` at `warn`.
  Silence a lint at its site with `#[expect(..., reason = "...")]`.
  The crate holds no `clippy.toml`.
- Hand-written `Display`, `Error`, `From`, and `AsRef` impls are the
  fallback. Derive them with `derive_more` (features `display`, `error`,
  `from`, `as_ref`).
- Keep `Cargo.lock` committed — the Nix build needs it and has no network.
- Only git-tracked files enter the Nix build; `git add` new files before
  `nix build` or the build fails mysteriously.
