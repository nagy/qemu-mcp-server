//! Buffered connection to the guest serial console.
//!
//! The background reader task stays connected between calls and keeps
//! every byte the guest prints, so a later read sees the whole output.

#![expect(
    clippy::redundant_pub_crate,
    reason = "the module stays private, and the crate root calls these helpers"
)]

use std::sync::Arc;

use rmcp::model::ErrorData as McpError;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixStream, unix::OwnedWriteHalf},
};

use crate::{QMPSocket, SocketPath};

/// Upper bound on buffered serial output; older bytes are dropped.
const SERIAL_BUFFER_LIMIT: usize = 256 * 1024;

/// State of the persistent background connection to the serial socket.
#[derive(Default)]
pub(crate) struct SerialState {
    /// Output bytes received since the buffer was last cleared.
    pub(crate) buffer: Vec<u8>,
    /// Whether the background reader task currently holds a connection.
    pub(crate) connected: bool,
    /// Write half of the connection, owned while connected.
    pub(crate) writer: Option<OwnedWriteHalf>,
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

impl QMPSocket {
    /// Write bytes to the open serial connection.
    pub(crate) async fn send_to_serial(&self, payload: &[u8]) -> Result<(), SerialError> {
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
    pub(crate) async fn ensure_serial_connected(&self) -> Result<(), SerialError> {
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
    pub(crate) async fn take_serial_output(
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

#[cfg(test)]
mod tests {
    use rmcp::{handler::server::wrapper::Parameters, model::CallToolResult};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    use crate::{QMPSocket, SerialReadRequest, SerialWriteRequest, SocketPath, test_support::bind};

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
