use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::tool::{Parameters, ToolRouter},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    serde_json::json,
    tool, tool_handler, tool_router,
    transport::stdio,
};

pub const QMP_SOCKET_PATH: &'static str = "/tmp/qmp-sock";

#[derive(Clone)]
pub struct QMPSocket {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MemoryRequest {
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
        }
    }
    #[tool(description = "You have access to one QMP socket only.")]
    async fn execute_qmp(
        &self,
        Parameters(MemoryRequest {
            qmp_command,
            qmp_arguments,
        }): Parameters<MemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !std::fs::exists(QMP_SOCKET_PATH).unwrap() {
            return Ok(CallToolResult::error(vec![Content::text(
                "No such socket exists.",
            )]));
        }
        let socket_addr = QMP_SOCKET_PATH;
        let stream = qapi::futures::QmpStreamTokio::open_uds(socket_addr)
            .await
            .unwrap();
        let stream = stream.negotiate().await.unwrap();
        let (qmp, handle) = stream.spawn_tokio();
        match qmp_command.as_str() {
            "query-status" => {
                let status = qmp.execute(qapi::qmp::query_status {}).await.unwrap();
                let result = CallToolResult::success(vec![
                    Content::json(json!({
                        "running": status.running,
                        "status": format!("{:#?}", status.status),
                    }))
                    .unwrap(),
                ]);
                return Ok(result);
            }
            "stop" => {
                qmp.execute(qapi::qmp::stop {}).await.unwrap();
                let result = CallToolResult::success(vec![Content::text("stopped")]);
                return Ok(result);
            }
            "cont" => {
                qmp.execute(qapi::qmp::cont {}).await.unwrap();
                let result = CallToolResult::success(vec![Content::text("continued")]);
                return Ok(result);
            }
            "eject" => {
                // qmp.execute(qapi::qmp::eject {
                //     device: Some("ide-cd0".to_string()),
                //     force: None,
                //     id: None,
                // })
                // .await
                // .unwrap();
                let result = CallToolResult::success(vec![Content::text("ejected")]);
                return Ok(result);
            }
            // TODO: add more commands here. There should be a dynamic
            // way to do this but it appears that qapi does not
            // support that yet.
            _ => {}
        };
        {
            // NOTE: this isn't necessary, but to manually ensure the stream closes...
            drop(qmp); // relinquish handle on the stream
            handle.await.unwrap(); // wait for event loop to exit
        }
        Ok(CallToolResult::error(vec![Content::text(
            "No such tool name exists.",
        )]))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for QMPSocket {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("QEMU machine manager".into()),
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
