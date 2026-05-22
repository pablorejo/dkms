//! Network helpers that all five Rust binaries reuse.
//!
//! ## `bind_reuse_addr`
//!
//! On Kubernetes, the pod retains its IP across container restarts. After a
//! crash the previous process leaves sockets in `TIME_WAIT`, and the
//! restarting process trips on `EADDRINUSE (os error 98)` until the kernel
//! reaps them (~60 s default). Setting `SO_REUSEADDR` lets the new socket
//! bind immediately, eliminating the CrashLoopBackOff sequence we observed
//! in iter-001/iter-002 on `dkms-915`, `dkms-1037` and `dkms-1180`.
//!
//! Pure tokio — no extra crate — via `TcpSocket::set_reuseaddr`.

use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpSocket};

/// Bind a TCP listener with `SO_REUSEADDR=1` set before bind.
///
/// Default backlog is 1024 (tokio's `DEFAULT_BACKLOG` matches what tonic
/// uses internally). Returns a ready-to-`accept()` `TcpListener`.
pub async fn bind_reuse_addr(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Parse an `"ip:port"` (or `"[::1]:port"`) string and call
/// [`bind_reuse_addr`] on it.
pub async fn bind_reuse_addr_str(addr: &str) -> std::io::Result<TcpListener> {
    let parsed: SocketAddr = addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    bind_reuse_addr(parsed).await
}
