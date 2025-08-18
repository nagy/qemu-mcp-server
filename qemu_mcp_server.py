from mcp.server.fastmcp import FastMCP
from qemu.qmp import QMPClient

mcp = FastMCP("QMPServer")

QMP_SOCKET_PATH = "/tmp/qmp-sock"


@mcp.tool()
async def qmp_execute(commandname: str, arguments: dict = dict()) -> dict:
    """You have access to one QMP socket only."""
    qmp = QMPClient("qemu-machine")
    await qmp.connect(QMP_SOCKET_PATH)
    res = await qmp.execute(cmd=commandname, arguments=arguments)
    await qmp.disconnect()
    return res


if __name__ == "__main__":
    mcp.run()
