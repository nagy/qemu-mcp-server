# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project

`qemu-mcp-server` — a Rust binary exposing QEMU instances over the Model
Context Protocol (MCP) via QMP. Implementation lives in `qemu_mcp_server.rs`;
`src/lib.rs` is a thin shim (`#[path]` include + re-exports) that gives the
crate a lib target so doctests can run.

## Build & test

Nix flake (no default.nix anymore):

```bash
nix flake check   # builds the package and runs cargo tests + doctests
nix build         # build the binary into ./result
nix fmt           # format flake, Rust, and TOML via treefmt (never run formatters standalone)
nix develop       # dev shell with cargo
nix build .#qemu-mcp-server-doc  # static rustdoc HTML in result/share/doc
```

## Conventions

- Rust edition 2024; rustfmt config lives in `flake.nix` (treefmt).
- Keep `Cargo.lock` committed — the Nix build needs it and has no network.
- Only git-tracked files enter the Nix build; `git add` new files before
  `nix build` or the build fails mysteriously.
