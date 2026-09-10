{
  description = "Model Context Protocol (MCP) server to interact with QEMU instances (crane)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      crane,
      treefmt-nix,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ treefmt-nix.flakeModule ];
      systems = [ "x86_64-linux" ];

      perSystem =
        {
          system,
          pkgs,
          lib,
          config,
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

          # Pure Rust: no -sys crates in Cargo.toml, no build.rs probing
          # pkg-config/bindgen/cmake — so no system libraries needed.
          libInputs = [ ];

          # No native build tools required either (see libInputs above).
          nativeBuildInputs = [ ];

          # No bindgen / dlopen'd C libs, hence no env vars.
          env = { };

          # Source as seen by cargo: everything tracked by git.
          src = craneLib.cleanCargoSource ./.;

          # Cargo.lock has no network access during the build; fetch deps
          # first, then feed them to cargo through the registry cache.
          cargoArtifacts = craneLib.buildDepsOnly {
            inherit src nativeBuildInputs;
            buildInputs = libInputs;
            inherit env;
          };

          pkg = craneLib.buildPackage {
            inherit
              src
              cargoArtifacts
              env
              ;
            buildInputs = libInputs;
            nativeBuildInputs = nativeBuildInputs;
            meta = {
              description = "Model Context Protocol (MCP) server to interact with QEMU instances";
              homepage = "https://github.com/nagy/qemu-mcp-server";
              license = lib.licenses.agpl3Plus;
              mainProgram = "qemu-mcp-server";
              maintainers = with lib.maintainers; [ nagy ];
            };
          };
        in
        {
          packages.qemu-mcp-server = pkg;
          packages.default = config.packages.qemu-mcp-server;

          checks.default = craneLib.cargoTest {
            inherit
              src
              cargoArtifacts
              nativeBuildInputs
              env
              ;
            buildInputs = libInputs;
          };

          apps.default = {
            type = "app";
            program = "${pkg}/bin/qemu-mcp-server";
            meta.description = "Model Context Protocol (MCP) server to interact with QEMU instances";
          };

          treefmt = {
            programs.rustfmt.enable = true;
            programs.rustfmt.edition = "2024";
            programs.taplo.enable = true;
            programs.nixfmt.enable = true;
            settings.formatter.rustfmt.options = lib.mkAfter [
              "--config"
              "max_width=100,comment_width=100,wrap_comments=false,group_imports=StdExternalCrate,imports_granularity=Crate,condense_wildcard_suffixes=true,format_code_in_doc_comments=true,format_macro_matchers=true,format_macro_bodies=true,format_strings=true,use_field_init_shorthand=true"
            ];
          };

          devShells.default = craneLib.devShell {
            packages = [
              config.treefmt.build.wrapper
            ]
            ++ nativeBuildInputs;
            buildInputs = libInputs;
            inherit env;
          };
        };
    };
}
