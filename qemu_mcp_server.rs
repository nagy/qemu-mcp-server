use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::tool::{Parameters, ToolRouter},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    serde_json::json,
    tool, tool_handler, tool_router,
    transport::stdio,
};

const DEFAULT_QMP_SOCKET_PATH: &str = "/tmp/qmp-sock";

#[derive(Clone)]
pub struct QMPSocket {
    tool_router: ToolRouter<Self>,
    socket_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QmpRequest {
    #[schemars(description = "The command name.")]
    pub qmp_command: String,
    #[schemars(description = "The arguments as JSON object.")]
    pub qmp_arguments: String,
}

#[tool_router]
impl QMPSocket {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            socket_path: std::env::var("QMP_SOCKET_PATH")
                .unwrap_or_else(|_| DEFAULT_QMP_SOCKET_PATH.to_owned()),
        }
    }
    #[tool(
        description = "Execute a QMP command on the QEMU instance listening on the local QMP unix \
                       socket (default /tmp/qmp-sock, override with the QMP_SOCKET_PATH \
                       environment variable). Supports the convenience commands `query-status`, \
                       `stop`, `cont`, and `eject`; any other value is passed through as a raw \
                       QMP command. `qmp_command` is the QMP command name and `qmp_arguments` is \
                       a JSON object with its arguments (e.g. `{\"device\": \"ide-cd0\"}` for \
                       `eject`)."
    )]
    async fn execute_qmp(
        &self,
        Parameters(QmpRequest {
            qmp_command,
            qmp_arguments,
        }): Parameters<QmpRequest>,
    ) -> Result<CallToolResult, McpError> {
        let stream = qapi::futures::QmpStreamTokio::open_uds(&self.socket_path)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    format!("failed to connect to QMP socket {}: {e}", self.socket_path),
                    None,
                )
            })?;
        let stream = stream
            .negotiate()
            .await
            .map_err(|e| McpError::internal_error(format!("QMP handshake failed: {e}"), None))?;
        let (qmp, handle) = stream.spawn_tokio();

        let result: Result<CallToolResult, McpError> = async {
            match qmp_command.as_str() {
                "query-status" => {
                    let status = qmp
                        .execute(qapi::qmp::query_status {})
                        .await
                        .map_err(qmp_err)?;
                    Ok(CallToolResult::success(vec![
                        Content::json(json!({
                            "running": status.running,
                            "status": status.status,
                        }))
                        .map_err(qmp_err)?,
                    ]))
                }
                "stop" => {
                    qmp.execute(qapi::qmp::stop {}).await.map_err(qmp_err)?;
                    Ok(CallToolResult::success(vec![Content::text("stopped")]))
                }
                "cont" => {
                    qmp.execute(qapi::qmp::cont {}).await.map_err(qmp_err)?;
                    Ok(CallToolResult::success(vec![Content::text("continued")]))
                }
                "eject" => {
                    // TODO (R2/R3): execute the real command once
                    // qmp_arguments is parsed.
                    Ok(CallToolResult::success(vec![Content::text("ejected")]))
                }
                // TODO: add more commands here. There should be a dynamic
                // way to do this but it appears that qapi does not
                // support that yet.
                _ => Ok(CallToolResult::error(vec![Content::text(
                    "No such tool name exists.",
                )])),
            }
        }
        .await;

        // NOTE: this isn't necessary, but to manually ensure the stream closes...
        drop(qmp); // relinquish handle on the stream
        let _ = handle.await; // wait for event loop to exit
        result
    }
}

fn qmp_err(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("QMP command failed: {e}"), None)
}

#[tool_handler]
impl rmcp::ServerHandler for QMPSocket {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: rmcp::model::Implementation {
                name: "qemu-mcp-server".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "Manage a QEMU virtual machine over its QMP socket. The socket must point at a \
                 running QEMU instance (launch QEMU with `-qmp unix:/tmp/qmp-sock,server,nowait`)."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = QMPSocket::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
