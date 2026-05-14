//! Binary TCP server for QKC↔QKC frames.
//!
//! Accepts connections, reads framed messages via
//! [`common::ipc::binary_tcp::read_frame`], and dispatches them to
//! [`QkcService`]. There's no encryption at this layer: the wire is assumed
//! protected by MACsec / PQC at L2.

use common::ipc::binary_tcp::{read_frame, write_frame, Frame, FRAME_RECV, FRAME_RELAY};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

use crate::service::QkcService;

pub async fn serve(svc: QkcService, bind: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&bind).await?;
    info!(%bind, "qkc TCP listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        let svc = svc.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(svc, stream, peer.to_string()).await {
                warn!(error = %e, %peer, "qkc tcp conn ended with error");
            }
        });
    }
}

async fn handle_conn(svc: QkcService, mut stream: TcpStream, peer: String) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    loop {
        let frame = read_frame(&mut stream).await?;
        match frame.kind {
            FRAME_RECV  => handle_recv(&svc, frame, &mut stream).await?,
            FRAME_RELAY => handle_relay(&svc, frame, &mut stream).await?,
            other       => warn!(%peer, %other, "unknown frame type, dropping"),
        }
    }
}

async fn handle_recv(svc: &QkcService, frame: Frame, _stream: &mut TcpStream) -> anyhow::Result<()> {
    // TODO: decrypt with KME keys (`frame.key_ids`), hand off to local DKMS.
    let _ = svc;
    let _ = frame;
    Ok(())
}

async fn handle_relay(svc: &QkcService, frame: Frame, _stream: &mut TcpStream) -> anyhow::Result<()> {
    // TODO: look up next hop via `svc.routing.next_hop(&frame.dest_final)`,
    //       re-encrypt with that link's keys, write_frame to outbound socket.
    let _ = svc;
    let _ = frame;
    Ok(())
}

/// Helper for tests: send a frame and drop the connection.
pub async fn send_one(addr: &str, frame: &Frame) -> anyhow::Result<()> {
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    write_frame(&mut s, frame).await?;
    Ok(())
}
