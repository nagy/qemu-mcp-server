//! MCP server that exposes a QEMU instance over QMP and its serial console.
//!
//! Build one [`QMPSocket`] value and serve it on any rmcp transport. The
//! [`run`] function is the stdio entry point used by the binary.
//!
//! QEMU must listen on two unix sockets, for example:
//!
//! ```text
//! qemu-system-x86_64 -qmp unix:/tmp/qmp-sock,server,nowait \
//!     -serial unix:/tmp/serial-sock,server,nowait
//! ```

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, ErrorCode, Implementation, ServerCapabilities, ServerConfig,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
    time::sleep,
};

const DEFAULT_QMP_SOCKET_PATH: &str = "/tmp/qmp-sock";
const DEFAULT_SERIAL_SOCKET_PATH: &str = "/tmp/serial-sock";
const DEFAULT_SERIAL_TIMEOUT_MS: u64 = 5000;
const DEFAULT_SERIAL_MAX_CHARS: usize = 4000;
/// Upper bound on buffered serial output; older bytes are dropped.
const SERIAL_BUFFER_LIMIT: usize = 256 * 1024;
/// How often `read_serial` re-checks the buffer while waiting.
const SERIAL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Filesystem path to a QEMU unix socket.
///
/// The wrapper keeps the QMP path and the serial path from swapping
/// places at a call site.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::SocketPath;
///
/// let qmp = SocketPath::from("/tmp/qmp-sock");
/// assert_eq!(qmp.as_str(), "/tmp/qmp-sock");
/// assert_eq!(qmp.to_string(), "/tmp/qmp-sock");
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, derive_more::Display, derive_more::From,
)]
pub struct SocketPath(String);

impl SocketPath {
    /// Wrap a socket path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrow the path as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SocketPath {
    fn from(path: &str) -> Self {
        Self(path.to_owned())
    }
}

impl AsRef<str> for SocketPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<std::path::Path> for SocketPath {
    fn as_ref(&self) -> &std::path::Path {
        self.0.as_ref()
    }
}

/// MCP server exposing a QEMU instance's QMP socket and serial console.
///
/// This type does not implement `Debug`, because [`ToolRouter`] does not.
#[derive(Clone)]
pub struct QMPSocket {
    tool_router: ToolRouter<Self>,
    socket_path: SocketPath,
    serial_socket_path: SocketPath,
    serial: Arc<Mutex<SerialState>>,
}

/// State of the persistent background connection to the serial socket.
#[derive(Default)]
struct SerialState {
    /// Output bytes received since the buffer was last cleared.
    buffer: Vec<u8>,
    /// Whether the background reader task currently holds a connection.
    connected: bool,
    /// Write half of the connection, owned while connected.
    writer: Option<OwnedWriteHalf>,
}

/// Failure while driving the guest's serial console.
///
/// [`McpError`] reports every variant as an internal error, so a caller
/// that only needs the text prints the value with `Display`.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::SerialError;
///
/// let error = SerialError::Lost;
/// assert_eq!(error.to_string(), "serial connection lost, retry the call");
/// ```
#[derive(Debug, derive_more::Display, derive_more::Error, derive_more::From)]
pub enum SerialError {
    /// The serial socket refused the connection.
    #[display("failed to connect to serial socket {path}: {source}")]
    Connect {
        /// Path the server tried to reach.
        path: SocketPath,
        /// Reason the connect call failed.
        #[error(source)]
        source: std::io::Error,
    },
    /// No background reader task holds a connection.
    #[display("serial connection lost, retry the call")]
    Lost,
    /// Writing the payload to the socket failed.
    #[display("failed to write to serial socket: {_0}")]
    Write(#[from] std::io::Error),
    /// The expected substring never appeared before the deadline.
    #[display(
        "timed out waiting for serial output containing {wait_for:?}; call again without \
         `wait_for` to see the raw buffer"
    )]
    Timeout {
        /// Substring that never appeared.
        wait_for: Option<String>,
    },
}

impl From<SerialError> for McpError {
    fn from(error: SerialError) -> Self {
        Self::internal_error(error.to_string(), None)
    }
}

/// Parameters of the `execute_qmp` tool.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::QmpRequest;
/// use serde_json::json;
///
/// let request: QmpRequest = serde_json::from_value(json!({
///     "qmp_command": "eject",
///     "qmp_arguments": { "device": "ide-cd0" }
/// }))
/// .unwrap();
/// assert_eq!(request.qmp_command, "eject");
/// assert_eq!(request.qmp_arguments, Some(json!({ "device": "ide-cd0" })));
///
/// // `qmp_arguments` may be omitted for commands that take no arguments.
/// let bare: QmpRequest =
///     serde_json::from_value(json!({ "qmp_command": "query-status" })).unwrap();
/// assert_eq!(bare.qmp_arguments, None);
/// ```
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct QmpRequest {
    /// The QMP command name, e.g. `query-status` or `query-block`.
    pub qmp_command: String,
    /// The command arguments as a JSON object.
    ///
    /// Omit this field for commands that take no arguments.
    pub qmp_arguments: Option<Value>,
}

/// Parameters of the `read_serial` tool.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::SerialReadRequest;
/// use serde_json::json;
///
/// let request: SerialReadRequest =
///     serde_json::from_value(json!({ "wait_for": "login:", "timeout_ms": 10_000 }))
///         .unwrap();
/// assert_eq!(request.wait_for.as_deref(), Some("login:"));
/// assert_eq!(request.timeout_ms, Some(10_000));
///
/// // Omitted fields fall back to the tool's defaults: no substring
/// // filter, 5000 ms timeout, 4000 chars, buffer kept.
/// let defaults: SerialReadRequest = serde_json::from_value(json!({})).unwrap();
/// assert_eq!(defaults.wait_for, None);
/// assert_eq!(defaults.timeout_ms, None);
/// assert_eq!(defaults.max_chars, None);
/// assert_eq!(defaults.clear, None);
/// ```
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct SerialReadRequest {
    /// Keep polling until this substring appears in the output, or give
    /// up after `timeout_ms`. Omit to return the pending output as-is.
    pub wait_for: Option<String>,
    /// Give up waiting for `wait_for` after this many milliseconds.
    pub timeout_ms: Option<u64>,
    /// Return at most this many characters from the end of the buffer.
    pub max_chars: Option<usize>,
    /// Discard the buffered output after returning it.
    pub clear: Option<bool>,
}

/// Parameters of the `write_serial` tool.
///
/// # Examples
///
/// ```
/// use qemu_mcp_server::SerialWriteRequest;
/// use serde_json::json;
///
/// let request: SerialWriteRequest =
///     serde_json::from_value(json!({ "data": "ls -l" })).unwrap();
/// assert_eq!(request.data, "ls -l");
/// // `newline` omitted means "append a newline" (the tool's default).
/// assert_eq!(request.newline, None);
///
/// let raw: SerialWriteRequest =
///     serde_json::from_value(json!({ "data": "ls -l\n", "newline": false })).unwrap();
/// assert_eq!(raw.newline, Some(false));
/// ```
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct SerialWriteRequest {
    /// Bytes to send to the guest's serial port, e.g. a command line.
    pub data: String,
    /// Append a newline after `data`.
    pub newline: Option<bool>,
}

impl Default for QMPSocket {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl QMPSocket {
    /// Build a server from the socket paths in the environment.
    ///
    /// Reads `QMP_SOCKET_PATH` and `SERIAL_SOCKET_PATH`. Uses the default
    /// socket paths when an environment variable is unset.
    #[must_use = "returns a new server; call `serve` to run it"]
    pub fn new() -> Self {
        Self::with_paths(
            std::env::var("QMP_SOCKET_PATH").unwrap_or_else(|_| DEFAULT_QMP_SOCKET_PATH.to_owned()),
            std::env::var("SERIAL_SOCKET_PATH")
                .unwrap_or_else(|_| DEFAULT_SERIAL_SOCKET_PATH.to_owned()),
        )
    }

    /// Build a server that uses the given QMP and serial socket paths.
    ///
    /// # Examples
    ///
    /// ```
    /// use qemu_mcp_server::{QMPSocket, SocketPath};
    ///
    /// let server = QMPSocket::with_paths("/tmp/qmp-sock", "/tmp/serial-sock");
    /// let _ = server;
    ///
    /// // `SocketPath` values make the argument order explicit.
    /// let qmp = SocketPath::new("/tmp/qmp-sock");
    /// let serial = SocketPath::new("/tmp/serial-sock");
    /// let server = QMPSocket::with_paths(qmp, serial);
    /// let _ = server;
    /// ```
    #[must_use = "returns a new server; call `serve` to run it"]
    pub fn with_paths(
        qmp_socket_path: impl Into<SocketPath>,
        serial_socket_path: impl Into<SocketPath>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            socket_path: qmp_socket_path.into(),
            serial_socket_path: serial_socket_path.into(),
            serial: Arc::new(Mutex::new(SerialState::default())),
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
        let ret = run_command(&self.socket_path, &qmp_command, qmp_arguments.as_ref()).await?;

        Ok(CallToolResult::success(vec![
            ContentBlock::json(ret).map_err(|e| {
                McpError::internal_error(format!("failed to serialize QMP result: {e}"), None)
            })?,
        ]))
    }

    #[tool(
        description = "Read pending output from the guest's serial console, connected to the \
                       local unix socket (default /tmp/serial-sock, override with the \
                       SERIAL_SOCKET_PATH environment variable; launch QEMU with e.g. `-serial \
                       unix:/tmp/serial-sock,server,nowait`). The server stays connected in the \
                       background and buffers everything the guest prints between calls, so \
                       nothing is lost while no read is in flight. With `wait_for`, keeps polling \
                       until the substring appears in the output or `timeout_ms` (default 5000) \
                       elapses. Without `wait_for`, returns the buffered output immediately \
                       (possibly empty). Output is returned as lossy UTF-8, capped at `max_chars` \
                       (default 4000) from the end of the buffer; pass `clear` to discard it \
                       after reading."
    )]
    async fn read_serial(
        &self,
        Parameters(request): Parameters<SerialReadRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Serve buffered output even if the connection has dropped;
        // only reconnect when there is nothing to report.
        {
            let state = self.serial.lock().await;
            if !state.connected && state.buffer.is_empty() {
                drop(state);
                self.ensure_serial_connected().await?;
            }
        }
        let deadline = Instant::now()
            + Duration::from_millis(request.timeout_ms.unwrap_or(DEFAULT_SERIAL_TIMEOUT_MS));
        let max_chars = request.max_chars.unwrap_or(DEFAULT_SERIAL_MAX_CHARS).max(1);
        let clear = request.clear.unwrap_or(false);
        loop {
            if let Some(output) = self
                .take_serial_output(request.wait_for.as_deref(), max_chars, clear)
                .await
            {
                return Ok(CallToolResult::success(vec![ContentBlock::text(output)]));
            }
            if Instant::now() >= deadline {
                return Err(SerialError::Timeout {
                    wait_for: request.wait_for,
                }
                .into());
            }
            sleep(SERIAL_POLL_INTERVAL).await;
        }
    }

    #[tool(
        description = "Write data to the guest's serial console (simulate typing into its UART \
                       over the same unix socket as `read_serial`). A newline is appended unless \
                       `newline` is false. Combine with `read_serial` (`wait_for` on an expected \
                       prompt) to drive interactive guest programs."
    )]
    async fn write_serial(
        &self,
        Parameters(request): Parameters<SerialWriteRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_serial_connected().await?;
        let mut payload = request.data.into_bytes();
        if request.newline.unwrap_or(true) {
            payload.push(b'\n');
        }
        self.send_to_serial(&payload).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]))
    }

    /// Write bytes to the open serial connection.
    async fn send_to_serial(&self, payload: &[u8]) -> Result<(), SerialError> {
        let mut state = self.serial.lock().await;
        let writer = state.writer.as_mut().ok_or(SerialError::Lost)?;
        writer.write_all(payload).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Connect to the serial socket and spawn the buffering reader task.
    ///
    /// Does nothing when a reader task already runs. Errors when the
    /// socket refuses the connection.
    async fn ensure_serial_connected(&self) -> Result<(), SerialError> {
        let mut state = self.serial.lock().await;
        if state.connected {
            return Ok(());
        }
        let socket = UnixStream::connect(&self.serial_socket_path)
            .await
            .map_err(|source| SerialError::Connect {
                path: self.serial_socket_path.clone(),
                source,
            })?;
        let (read_half, write_half) = socket.into_split();
        state.connected = true;
        state.writer = Some(write_half);
        let shared = Arc::clone(&self.serial);
        tokio::spawn(async move {
            let mut read_half = read_half;
            let mut chunk = [0u8; 4096];
            loop {
                match read_half.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut state = shared.lock().await;
                        state.buffer.extend_from_slice(&chunk[..n]);
                        if state.buffer.len() > SERIAL_BUFFER_LIMIT {
                            let excess = state.buffer.len() - SERIAL_BUFFER_LIMIT;
                            state.buffer.drain(..excess);
                        }
                    }
                }
            }
            let mut state = shared.lock().await;
            state.connected = false;
            state.writer = None;
        });
        Ok(())
    }

    /// Return the buffered serial output.
    ///
    /// Waits for `wait_for` when given. Clears the buffer when `clear`
    /// is set.
    async fn take_serial_output(
        &self,
        wait_for: Option<&str>,
        max_chars: usize,
        clear: bool,
    ) -> Option<String> {
        let mut state = self.serial.lock().await;
        let text = String::from_utf8_lossy(&state.buffer);
        if wait_for.is_some_and(|needle| !text.contains(needle)) {
            return None;
        }
        let start = text.len().saturating_sub(max_chars);
        let start = (start..=text.len())
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(start);
        let output = text[start..].to_owned();
        if clear {
            state.buffer.clear();
        }
        Some(output)
    }
}

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
async fn run_command(
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
async fn qmp_read_message(reader: &mut BufReader<OwnedReadHalf>) -> Result<Value, QmpError> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(QmpError::Closed);
    }
    Ok(serde_json::from_str(&line)?)
}

/// Build the JSON wire request for one QMP command.
fn qmp_request(command: &str, arguments: Option<&Value>, id: u64) -> Value {
    let mut request = json!({ "execute": command, "id": id });
    if let Some(arguments) = arguments {
        request["arguments"] = arguments.clone();
    }
    request
}

/// Send one QMP command and await its response.
///
/// Skips asynchronous events.
async fn qmp_execute(
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
async fn qmp_negotiate(
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

#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp's tool_handler macro generates the async trait impl"
)]
#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for QMPSocket {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "qemu-mcp-server",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Manage a QEMU virtual machine over its QMP socket and drive its serial console. \
                 Launch QEMU with `-qmp unix:/tmp/qmp-sock,server,nowait` and, for the serial \
                 tools, `-serial unix:/tmp/serial-sock,server,nowait`. Socket paths can be \
                 overridden with QMP_SOCKET_PATH and SERIAL_SOCKET_PATH.",
            )
    }
}

/// Run the server on stdio using [`QMPSocket::new`].
///
/// This is the entry point for the `qemu-mcp-server` binary. Callers
/// that need custom socket paths or a different transport construct
/// [`QMPSocket`] directly and use [`rmcp::ServiceExt::serve`].
///
/// # Errors
///
/// Returns an error when the stdio transport fails to start, when the
/// MCP handshake fails, or when the served connection ends in a
/// transport error.
pub async fn run() -> anyhow::Result<()> {
    let server = QMPSocket::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
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

    fn bind(name: &str) -> (UnixListener, std::path::PathBuf) {
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

    /// Build a server instance wired to the given test socket paths.
    fn server_with_paths(qmp: &std::path::Path, serial: &std::path::Path) -> QMPSocket {
        QMPSocket::with_paths(
            SocketPath::from(qmp.display().to_string()),
            SocketPath::from(serial.display().to_string()),
        )
    }

    fn serial_read_request(wait_for: Option<&str>) -> SerialReadRequest {
        SerialReadRequest {
            wait_for: wait_for.map(str::to_owned),
            timeout_ms: Some(2000),
            max_chars: None,
            clear: None,
        }
    }

    /// Fake guest: prints a boot banner, reads one line, echoes a
    /// canned reply.
    async fn serve_serial(listener: UnixListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        write_half
            .write_all(b"Booting ESP32...\nBoot OK\n")
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "help\n");
        write_half.write_all(b"echo: hi\n").await.unwrap();
        drop(write_half); // let the client see EOF
    }

    /// Extract the text of a tool result's first content item.
    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn serial_output_is_buffered_and_writable() {
        let (_, qmp_path) = bind("serial-qmp-unused");
        let (listener, serial_path) = bind("serial-main");
        let fake = tokio::spawn(serve_serial(listener));
        let server = server_with_paths(&qmp_path, &serial_path);

        let result = server
            .read_serial(Parameters(serial_read_request(Some("Boot OK"))))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("Booting ESP32..."));
        assert!(text.contains("Boot OK"));

        let request = SerialWriteRequest {
            data: "help".to_owned(),
            newline: None,
        };
        server.write_serial(Parameters(request)).await.unwrap();

        let result = server
            .read_serial(Parameters(serial_read_request(Some("echo: hi"))))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("echo: hi"));

        drop(server);
        fake.await.unwrap();
        let _ = std::fs::remove_file(&serial_path);
        let _ = std::fs::remove_file(&qmp_path);
    }

    #[tokio::test]
    async fn serial_read_times_out_without_a_match() {
        let (_, qmp_path) = bind("serial-qmp-unused2");
        let (listener, serial_path) = bind("serial-timeout");
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut write_half) = stream.into_split();
            write_half.write_all(b"nothing relevant\n").await.unwrap();
            drop(write_half); // client sees EOF, buffer must survive
        });
        let server = server_with_paths(&qmp_path, &serial_path);

        let err = server
            .read_serial(Parameters(serial_read_request(Some("never printed"))))
            .await
            .unwrap_err();
        assert!(err.message.contains("timed out"));

        drop(server);
        fake.await.unwrap();
        let _ = std::fs::remove_file(&serial_path);
        let _ = std::fs::remove_file(&qmp_path);
    }

    #[tokio::test]
    async fn serial_clear_drops_the_buffer() {
        let (_, qmp_path) = bind("serial-qmp-unused3");
        let (listener, serial_path) = bind("serial-clear");
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut write_half) = stream.into_split();
            write_half.write_all(b"one two\n").await.unwrap();
            drop(write_half);
            // Keep the socket alive until the test is done, so the
            // client can reconnect.
            let _ = done_rx.await;
        });
        let server = server_with_paths(&qmp_path, &serial_path);

        let mut request = serial_read_request(Some("one two"));
        request.clear = Some(true);
        server.read_serial(Parameters(request)).await.unwrap();

        let result = server
            .read_serial(Parameters(serial_read_request(None)))
            .await
            .unwrap();
        assert_eq!(result_text(&result), "");

        drop(server);
        let _ = done_tx.send(());
        fake.await.unwrap();
        let _ = std::fs::remove_file(&serial_path);
        let _ = std::fs::remove_file(&qmp_path);
    }
}
