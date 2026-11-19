//! Unix socket paths for the QMP and serial connections.

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
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    derive_more::Display,
    derive_more::From,
    derive_more::AsRef,
)]
#[from(String, &str)]
#[as_ref(str, std::path::Path)]
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
