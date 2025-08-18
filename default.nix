{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  python3 ? pkgs.python3,
}:

python3.pkgs.buildPythonApplication rec {
  pname = "qemu-mcp-server";
  version = "0-unstable-2025-08-18";
  pyproject = true;

  src = lib.cleanSource ./.;

  nativeBuildInputs = [
    python3.pkgs.hatchling
    python3.pkgs.qemu
    python3.pkgs.mcp
  ];

  pythonImportsCheck = [ "qemu_mcp_server" ];

  meta = {
    description = "Model Context Protocol (MCP) server to interact with QEMU instances";
    homepage = "https://github.com/nagy/qemu-mcp-server";
    license = lib.licenses.agpl3Plus;
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "qemu-mcp-server";
  };
}
