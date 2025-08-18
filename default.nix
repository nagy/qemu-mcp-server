{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "qemu-mcp-server";
  version = "0-unstable-2025-08-18";

  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  meta = {
    description = "Model Context Protocol (MCP) server to interact with QEMU instances";
    homepage = "https://github.com/nagy/qemu-mcp-server";
    license = lib.licenses.agpl3Plus;
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "qemu-mcp-server";
  };
}
