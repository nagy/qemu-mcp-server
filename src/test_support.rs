//! Shared fixtures for the unit tests.
//!
//! This module sits behind `#[cfg(test)]`, so it never enters the binary.

use std::path::{Path, PathBuf};

use tokio::{
    io::BufReader,
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

pub fn socket_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("qemu-mcp-server-test-{name}.sock"))
}

pub fn bind(name: &str) -> (UnixListener, PathBuf) {
    let path = socket_path(name);
    let _ = std::fs::remove_file(&path);
    (UnixListener::bind(&path).unwrap(), path)
}

pub async fn connect(path: &Path) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let socket = UnixStream::connect(path).await.unwrap();
    let (read_half, write_half) = socket.into_split();
    (BufReader::new(read_half), write_half)
}
