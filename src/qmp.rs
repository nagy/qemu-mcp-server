//! QMP client for one QEMU instance.
//!
//! Each command opens the unix socket, completes the handshake, sends
//! one request, and closes the connection again.

#![expect(
    clippy::redundant_pub_crate,
    reason = "the module stays private, and the crate root calls these helpers"
)]

use rmcp::model::{ErrorCode, ErrorData as McpError};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::SocketPath;

/// Failure while talking QMP to the QEMU instance.
///
/// [`McpError`] maps [`QmpError::CommandNotFound`] to `METHOD_NOT_FOUND`,
/// [`QmpError::Command`] to `INVALID_PARAMS`, and the rest to an internal
/// error.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::QmpError;
/// use rmcp::model::ErrorCode;
///
/// let error = QmpError::CommandNotFound("frobnicate".to_owned());
/// assert_eq!(error.to_string(), "QMP command not found: frobnicate");
/// assert_eq!(
///     rmcp::model::ErrorData::from(error).code,
///     ErrorCode::METHOD_NOT_FOUND
/// );
/// ```
#[derive(Debug, derive_more::Display, derive_more::Error, derive_more::From)]
pub enum QmpError {
    /// The QMP socket refused the connection.
    #[display("failed to connect to QMP socket {path}: {source}")]
    Connect {
        /// Path the server tried to reach.
        path: SocketPath,
        /// Reason the connect call failed.
        #[error(source)]
        source: std::io::Error,
    },
    /// Reading from or writing to the socket failed.
    #[display("QMP transport error: {_0}")]
    Transport(#[from] std::io::Error),
    /// QEMU closed the socket.
    #[display("QMP connection closed by QEMU")]
    Closed,
    /// A line from QEMU is not valid JSON.
    #[display("malformed QMP message: {_0}")]
    Malformed(#[from] serde_json::Error),
    /// The first message is not a QMP greeting.
    #[display("expected QMP greeting, got: {_0}")]
    BadGreeting(#[error(ignore)] Value),
    /// QEMU does not know the command.
    #[display("QMP command not found: {_0}")]
    CommandNotFound(#[error(ignore)] String),
    /// QEMU rejected the command.
    #[display("QMP error ({class}): {desc}")]
    Command {
        /// QMP error class reported by QEMU.
        class: String,
        /// Human-readable description reported by QEMU.
        desc: String,
    },
}

impl From<QmpError> for McpError {
    fn from(error: QmpError) -> Self {
        let message = error.to_string();
        match error {
            QmpError::CommandNotFound(_) => Self::new(ErrorCode::METHOD_NOT_FOUND, message, None),
            QmpError::Command { .. } => Self::invalid_params(message, None),
            _ => Self::internal_error(message, None),
        }
    }
}

/// Connect to the QMP socket, negotiate, and execute one command.
///
/// The connection is always shut down before returning, on success and
/// on failure alike.
pub(crate) async fn run_command(
    socket_path: &SocketPath,
    command: &str,
    arguments: Option<&Value>,
) -> Result<Value, QmpError> {
    let socket = UnixStream::connect(socket_path)
        .await
        .map_err(|source| QmpError::Connect {
            path: socket_path.clone(),
            source,
        })?;
    let (read_half, mut write_half) = socket.into_split();
    let mut reader = BufReader::new(read_half);

    let result = async {
        qmp_negotiate(&mut reader, &mut write_half).await?;
        qmp_execute(&mut reader, &mut write_half, command, arguments, 1).await
    }
    .await;
    let _ = write_half.shutdown().await;
    result
}

/// Read a single line from the QMP socket and parse it as JSON.
pub(crate) async fn qmp_read_message(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<Value, QmpError> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(QmpError::Closed);
    }
    Ok(serde_json::from_str(&line)?)
}

/// Build the JSON wire request for one QMP command.
pub(crate) fn qmp_request(command: &str, arguments: Option<&Value>, id: u64) -> Value {
    let mut request = json!({ "execute": command, "id": id });
    if let Some(arguments) = arguments {
        request["arguments"] = arguments.clone();
    }
    request
}

/// Send one QMP command and await its response.
///
/// Skips asynchronous events.
pub(crate) async fn qmp_execute(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    command: &str,
    arguments: Option<&Value>,
    id: u64,
) -> Result<Value, QmpError> {
    let request = qmp_request(command, arguments, id);
    writer.write_all(format!("{request}\n").as_bytes()).await?;
    writer.flush().await?;

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
            return Err(if class == "CommandNotFound" {
                QmpError::CommandNotFound(command.to_owned())
            } else {
                QmpError::Command {
                    class: class.to_owned(),
                    desc: desc.to_owned(),
                }
            });
        }
        if let Some(ret) = message.get("return") {
            return Ok(ret.clone());
        }
    }
}

/// Perform the QMP handshake.
///
/// Reads the greeting, then completes `qmp_capabilities` negotiation.
pub(crate) async fn qmp_negotiate(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) -> Result<(), QmpError> {
    let greeting = qmp_read_message(reader).await?;
    if greeting.get("QMP").is_none() {
        return Err(QmpError::BadGreeting(greeting));
    }
    qmp_execute(reader, writer, "qmp_capabilities", None, 0).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    use super::*;
    use crate::test_support::{bind, connect};

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

    #[tokio::test]
    async fn passthrough_roundtrip_with_negotiation() {
        let (listener, path) = bind("roundtrip");
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
        let (listener, path) = bind("events");
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
        let (listener, path) = bind("command-not-found");
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
        assert!(matches!(err, QmpError::CommandNotFound(ref command) if command == "frobnicate"));
        let mcp = McpError::from(err);
        assert_eq!(mcp.code, ErrorCode::METHOD_NOT_FOUND);
        assert!(mcp.message.contains("frobnicate"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn qmp_error_maps_to_invalid_params() {
        let (listener, path) = bind("device-not-found");
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
        assert!(matches!(
            err,
            QmpError::Command { ref class, ref desc }
                if class == "DeviceNotFound" && desc.contains("ide-cd0")
        ));
        let mcp = McpError::from(err);
        assert_eq!(mcp.code, ErrorCode::INVALID_PARAMS);
        assert!(mcp.message.contains("DeviceNotFound"));
        assert!(mcp.message.contains("ide-cd0"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn negotiate_rejects_a_bad_greeting() {
        let (listener, path) = bind("bad-greeting");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut write_half) = stream.into_split();
            write_half.write_all(b"{\"hello\": true}\n").await.unwrap();
        });
        let (mut reader, mut writer) = connect(&path).await;
        let err = qmp_negotiate(&mut reader, &mut writer).await.unwrap_err();
        assert!(matches!(err, QmpError::BadGreeting(_)));
        assert!(err.to_string().contains("expected QMP greeting"));
        drop(reader);
        drop(writer);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
