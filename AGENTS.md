# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project

`qemu-mcp-server` — a Rust binary exposing QEMU instances over the Model
Context Protocol (MCP) via QMP. Single source file: `qemu_mcp_server.rs`.

## Build & test

Nix flake (no default.nix anymore):

```bash
nix flake check   # builds the package and runs cargo tests
nix build         # build the binary into ./result
nix fmt           # format flake, Rust, and TOML via treefmt (never run formatters standalone)
nix develop       # dev shell with cargo
```

## Conventions

- Rust edition 2024; rustfmt config lives in `flake.nix` (treefmt).
- Keep `Cargo.lock` committed — the Nix build needs it and has no network.
- Only git-tracked files enter the Nix build; `git add` new files before
  `nix build` or the build fails mysteriously.
