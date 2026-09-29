#[tokio::main]
async fn main() -> anyhow::Result<()> {
    qemu_mcp_server::run().await
}
