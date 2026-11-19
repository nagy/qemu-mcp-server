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
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde_json::Value;
use tokio::{sync::Mutex, time::sleep};

mod path;
mod qmp;
mod serial;
#[cfg(test)]
mod test_support;

pub use path::SocketPath;
pub use qmp::QmpError;
use qmp::run_command;
pub use serial::SerialError;
use serial::SerialState;

const DEFAULT_QMP_SOCKET_PATH: &str = "/tmp/qmp-sock";
const DEFAULT_SERIAL_SOCKET_PATH: &str = "/tmp/serial-sock";
const DEFAULT_SERIAL_TIMEOUT_MS: u64 = 5000;
const DEFAULT_SERIAL_MAX_CHARS: usize = 4000;
/// How often `read_serial` re-checks the buffer while waiting.
const SERIAL_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
