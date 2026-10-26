{
  description = "Model Context Protocol (MCP) server to interact with QEMU instances";

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
      crane,
      treefmt-nix,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ treefmt-nix.flakeModule ];
      systems = [ "x86_64-linux" ];

      perSystem =
        {
          pkgs,
          lib,
          config,
          ...
        }:
        let
          craneLib = crane.mkLib pkgs;

          src = craneLib.cleanCargoSource ./.;

          cargoArtifacts = craneLib.buildDepsOnly { inherit src; };
        in
        {
          packages = {
            qemu-mcp-server = craneLib.buildPackage {
              inherit src cargoArtifacts;
              meta = {
                description = "Model Context Protocol (MCP) server to interact with QEMU instances";
                homepage = "https://github.com/nagy/qemu-mcp-server";
                license = lib.licenses.agpl3Plus;
                mainProgram = "qemu-mcp-server";
                maintainers = with lib.maintainers; [ nagy ];
              };
            };
            default = config.packages.qemu-mcp-server;

            qemu-mcp-server-doc = craneLib.cargoDoc {
              inherit src cargoArtifacts;
              meta.description = "QEMU MCP server API documentation";
            };
          };

          checks = {
            default = craneLib.cargoTest {
              inherit src cargoArtifacts;
              meta.description = "QEMU MCP server test suite";
            };

            clippy = craneLib.cargoClippy {
              inherit src cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              meta.description = "QEMU MCP server clippy check";
            };

            doctests = craneLib.cargoDocTest {
              inherit src cargoArtifacts;
              meta.description = "QEMU MCP server doctest suite";
            };
          };

          apps.default = {
            type = "app";
            program = "${config.packages.qemu-mcp-server}/bin/qemu-mcp-server";
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
            packages = [ config.treefmt.build.wrapper ];
          };
        };
    };
}
