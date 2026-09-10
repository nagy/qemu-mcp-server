use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::tool::{Parameters, ToolRouter},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

const DEFAULT_QMP_SOCKET_PATH: &str = "/tmp/qmp-sock";

/// MCP server exposing a QEMU instance's QMP socket.
#[derive(Clone)]
pub struct QMPSocket {
    tool_router: ToolRouter<Self>,
    socket_path: String,
}

/// Parameters of the `execute_qmp` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QmpRequest {
    /// The QMP command name, e.g. `query-status` or `query-block`.
    pub qmp_command: String,
    /// The command arguments as a JSON object; omit for commands that
    /// take no arguments.
    pub qmp_arguments: Option<Value>,
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
                       environment variable). Any QMP command passes through, e.g. \
                       `query-status`, `stop`, `cont`, `eject`, `query-block`. `qmp_command` is \
                       the command name and `qmp_arguments` is an optional JSON object with its \
                       arguments (e.g. {\"device\": \"ide-cd0\"} for `eject`)."
    )]
    async fn execute_qmp(
        &self,
        Parameters(QmpRequest {
            qmp_command,
            qmp_arguments,
        }): Parameters<QmpRequest>,
    ) -> Result<CallToolResult, McpError> {
        let socket = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            McpError::internal_error(
                format!("failed to connect to QMP socket {}: {e}", self.socket_path),
                None,
            )
        })?;
        let (read_half, mut write_half) = socket.into_split();
        let mut reader = BufReader::new(read_half);

        qmp_negotiate(&mut reader, &mut write_half).await?;
        let result = qmp_execute(
            &mut reader,
            &mut write_half,
            &qmp_command,
            qmp_arguments.as_ref(),
            1,
        )
        .await;
        let _ = write_half.shutdown().await;
        let ret = result?;

        Ok(CallToolResult::success(vec![Content::json(ret).map_err(
            |e| McpError::internal_error(format!("failed to serialize QMP result: {e}"), None),
        )?]))
    }
}

/// Read a single line from the QMP socket and parse it as JSON.
async fn qmp_read_message(reader: &mut BufReader<OwnedReadHalf>) -> Result<Value, McpError> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| McpError::internal_error(format!("QMP connection error: {e}"), None))?;
    if n == 0 {
        return Err(McpError::internal_error(
            "QMP connection closed by QEMU",
            None,
        ));
    }
    serde_json::from_str(&line)
        .map_err(|e| McpError::internal_error(format!("malformed QMP message: {e}"), None))
}

/// Build the JSON wire request for one QMP command.
fn qmp_request(command: &str, arguments: Option<&Value>, id: u64) -> Value {
    let mut request = json!({ "execute": command, "id": id });
    if let Some(arguments) = arguments {
        request["arguments"] = arguments.clone();
    }
    request
}

/// Send one QMP command and await its response, skipping asynchronous
/// events.
async fn qmp_execute(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    command: &str,
    arguments: Option<&Value>,
    id: u64,
) -> Result<Value, McpError> {
    let request = qmp_request(command, arguments, id);
    writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(|e| McpError::internal_error(format!("failed to send QMP command: {e}"), None))?;
    writer
        .flush()
        .await
        .map_err(|e| McpError::internal_error(format!("failed to send QMP command: {e}"), None))?;

    loop {
        let message = qmp_read_message(reader).await?;
        if message.get("event").is_some() {
            continue; // asynchronous event, not a response
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue; // response to some other in-flight command
        }
        if let Some(error) = message.get("error") {
            let class = error
                .get("class")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let desc = error.get("desc").and_then(Value::as_str).unwrap_or("");
            return Err(match class {
                "CommandNotFound" => McpError::new(
                    rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                    format!("QMP command not found: {command}"),
                    None,
                ),
                _ => McpError::invalid_params(format!("QMP error ({class}): {desc}"), None),
            });
        }
        if let Some(ret) = message.get("return") {
            return Ok(ret.clone());
        }
    }
}

/// Perform the QMP handshake: read the greeting and complete
/// `qmp_capabilities` negotiation.
async fn qmp_negotiate(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) -> Result<(), McpError> {
    let greeting = qmp_read_message(reader).await?;
    if !greeting.get("QMP").is_some() {
        return Err(McpError::internal_error(
            format!("expected QMP greeting, got: {greeting}"),
            None,
        ));
    }
    qmp_execute(reader, writer, "qmp_capabilities", None, 0).await?;
    Ok(())
}

#[tool_handler]
impl rmcp::ServerHandler for QMPSocket {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: rmcp::model::Implementation {
                name: "qemu-mcp-server".into(),
                version: env!("CARGO_PKG_VERSION").into(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rmcp::model::ErrorCode;
    use tokio::{io::AsyncWriteExt, net::UnixListener};

    use super::*;

    #[test]
    fn request_without_arguments_omits_the_field() {
        assert_eq!(
            qmp_request("query-status", None, 7),
            json!({ "execute": "query-status", "id": 7 })
        );
    }

    #[test]
    fn request_embeds_arguments() {
        let args = json!({ "device": "ide-cd0" });
        assert_eq!(
            qmp_request("eject", Some(&args), 1),
            json!({ "execute": "eject", "id": 1, "arguments": { "device": "ide-cd0" } })
        );
    }

    /// One scripted fake-QEMU server action: either push an
    /// unsolicited event, or read one request and answer with a canned
    /// response (the request's `id` is filled in automatically).
    enum Step {
        Event(Value),
        Respond(Value),
    }

    /// Fake QEMU: greeting, then scripted steps, then drain remaining
    /// requests until the client disconnects.
    async fn serve(listener: UnixListener, steps: Vec<Step>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        write_half
            .write_all(
                concat!(
                    r#"{"QMP": {"version": {"qemu": {"micro": 0, "minor": 0, "major": 0}, "#,
                    r#""package": "fake"}}, "capabilities": []}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        for step in steps {
            match step {
                Step::Event(event) => {
                    write_half
                        .write_all(format!("{event}\n").as_bytes())
                        .await
                        .unwrap();
                }
                Step::Respond(mut response) => {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let request: Value = serde_json::from_str(&line).unwrap();
                    response["id"] = request["id"].clone();
                    write_half
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        }
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            line.clear();
        }
    }

    fn socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("qemu-mcp-server-test-{name}.sock"))
    }

    async fn bind(name: &str) -> (UnixListener, std::path::PathBuf) {
        let path = socket_path(name);
        let _ = std::fs::remove_file(&path);
        (UnixListener::bind(&path).unwrap(), path)
    }

    async fn connect(path: &std::path::Path) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let socket = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = socket.into_split();
        (BufReader::new(read_half), write_half)
    }

    #[tokio::test]
    async fn passthrough_roundtrip_with_negotiation() {
        let (listener, path) = bind("roundtrip").await;
        let server = tokio::spawn(serve(
            listener,
            vec![
                Step::Respond(json!({ "return": {} })),
                Step::Respond(json!({ "return": { "running": true, "status": "running" } })),
            ],
        ));
        let (mut reader, mut writer) = connect(&path).await;
        qmp_negotiate(&mut reader, &mut writer).await.unwrap();
        let ret = qmp_execute(&mut reader, &mut writer, "query-status", None, 1)
            .await
            .unwrap();
        assert_eq!(ret, json!({ "running": true, "status": "running" }));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn asynchronous_events_are_skipped() {
        let (listener, path) = bind("events").await;
        let server = tokio::spawn(serve(
            listener,
            vec![
                Step::Respond(json!({ "return": {} })),
                Step::Event(json!({ "event": "RESET", "data": {} })),
                Step::Respond(json!({ "return": { "status": "paused" } })),
            ],
        ));
        let (mut reader, mut writer) = connect(&path).await;
        qmp_negotiate(&mut reader, &mut writer).await.unwrap();
        let ret = qmp_execute(&mut reader, &mut writer, "query-status", None, 1)
            .await
            .unwrap();
        assert_eq!(ret, json!({ "status": "paused" }));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn command_not_found_maps_to_method_not_found() {
        let (listener, path) = bind("command-not-found").await;
        let server = tokio::spawn(serve(
            listener,
            vec![
                Step::Respond(json!({ "return": {} })),
                Step::Respond(json!({
                    "error": {
                        "class": "CommandNotFound",
                        "desc": "The command frobnicate has not been found"
                    }
                })),
            ],
        ));
        let (mut reader, mut writer) = connect(&path).await;
        qmp_negotiate(&mut reader, &mut writer).await.unwrap();
        let err = qmp_execute(&mut reader, &mut writer, "frobnicate", None, 1)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
        assert!(err.message.contains("frobnicate"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn qmp_error_maps_to_invalid_params() {
        let (listener, path) = bind("device-not-found").await;
        let server = tokio::spawn(serve(
            listener,
            vec![
                Step::Respond(json!({ "return": {} })),
                Step::Respond(json!({
                    "error": {
                        "class": "DeviceNotFound",
                        "desc": "Device 'ide-cd0' not found"
                    }
                })),
            ],
        ));
        let (mut reader, mut writer) = connect(&path).await;
        qmp_negotiate(&mut reader, &mut writer).await.unwrap();
        let err = qmp_execute(
            &mut reader,
            &mut writer,
            "eject",
            Some(&json!({ "device": "ide-cd0" })),
            1,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("DeviceNotFound"));
        assert!(err.message.contains("ide-cd0"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn negotiate_rejects_a_bad_greeting() {
        let (listener, path) = bind("bad-greeting").await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut write_half) = stream.into_split();
            write_half.write_all(b"{\"hello\": true}\n").await.unwrap();
        });
        let (mut reader, mut writer) = connect(&path).await;
        let err = qmp_negotiate(&mut reader, &mut writer).await.unwrap_err();
        assert!(err.message.contains("expected QMP greeting"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
