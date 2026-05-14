//! `qkc-test-client` — simula un ORR.
//!
//! Tres subcomandos:
//!
//! ```text
//!   qkc-test-client send    --addr 127.0.0.1:7101 --dest 3 --message "hola"
//!   qkc-test-client listen  --addr 127.0.0.1:7103
//!   qkc-test-client stress  --addr 127.0.0.1:7101 --dest 3 --count 10000 --bytes 64
//! ```
//!
//! * `send` abre TCP al `local_listen` de un QKC, mete un
//!   `FRAME_LOCAL_SEND` con `dest_final` + payload, y cierra.
//! * `listen` abre TCP al `local_listen` de un QKC, se queda escuchando
//!   `FRAME_LOCAL_DELIVER` y los imprime.
//! * `stress` manda N frames con un payload aleatorio y reporta
//!   throughput. Si pasas `--wait-deliver-on <addr>`, se conecta
//!   también al QKC destino y espera a recibir N deliveries antes de
//!   reportar latencia end-to-end.

use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::net::TcpStream;
use wire::{read_frame, write_frame, Frame, FRAME_LOCAL_DELIVER, FRAME_LOCAL_SEND};

#[derive(Parser, Debug)]
#[command(name = "qkc-test-client", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manda UN solo frame LOCAL_SEND y cierra.
    Send {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        dest: u32,
        #[arg(long, default_value = "hello")]
        message: String,
    },
    /// Mantiene conexión abierta al local_listen y cuenta deliveries.
    Listen {
        #[arg(long)]
        addr: String,
        /// Si se indica, sale tras recibir N frames (útil para measure-end-to-end).
        #[arg(long)]
        count: Option<u64>,
        /// Verbose: imprime cada frame que llega.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Manda N frames lo más rápido posible y reporta throughput.
    Stress {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        dest: u32,
        #[arg(long, default_value_t = 1000)]
        count: u64,
        /// Tamaño del payload por frame en bytes.
        #[arg(long, default_value_t = 64)]
        bytes: usize,
        /// Si se da, se conecta también al destino para medir
        /// latencia end-to-end (esperando que lleguen los N frames).
        #[arg(long)]
        wait_deliver_on: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Send { addr, dest, message } => do_send(&addr, dest, message.into_bytes()).await,
        Cmd::Listen { addr, count, verbose } => do_listen(&addr, count, verbose).await,
        Cmd::Stress { addr, dest, count, bytes, wait_deliver_on } => {
            do_stress(&addr, dest, count, bytes, wait_deliver_on).await
        }
    }
}

async fn do_send(addr: &str, dest: u32, payload: Vec<u8>) -> Result<()> {
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    let frame = build_local_send(dest, payload);
    write_frame(&mut s, &frame).await?;
    println!("sent OK to {addr}");
    Ok(())
}

async fn do_listen(addr: &str, count: Option<u64>, verbose: bool) -> Result<()> {
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    println!("listening on {addr}");
    let mut n: u64 = 0;
    let start = Instant::now();
    loop {
        let f = match read_frame(&mut s).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("listen ended: {e}");
                break;
            }
        };
        if f.kind != FRAME_LOCAL_DELIVER {
            if verbose {
                eprintln!("unexpected kind {:#x}", f.kind);
            }
            continue;
        }
        n += 1;
        if verbose {
            println!(
                "[{n}] sender={} dest_final={} bytes={} payload[..min(16)]={:02x?}",
                f.sender_id,
                f.dest_final,
                f.payload.len(),
                &f.payload[..f.payload.len().min(16)],
            );
        }
        if let Some(c) = count {
            if n >= c {
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "received {n} frames in {:.3}s ({:.0} fps)",
        elapsed,
        n as f64 / elapsed.max(1e-9),
    );
    Ok(())
}

async fn do_stress(
    addr: &str,
    dest: u32,
    count: u64,
    bytes: usize,
    wait_deliver_on: Option<String>,
) -> Result<()> {
    // Si se ha pedido medir end-to-end, lanza el listener primero en
    // un task aparte.
    let listener_handle = if let Some(deliver_addr) = wait_deliver_on.clone() {
        let target = count;
        Some(tokio::spawn(async move {
            let mut s = TcpStream::connect(&deliver_addr).await?;
            s.set_nodelay(true)?;
            let mut n: u64 = 0;
            let start = Instant::now();
            while n < target {
                match read_frame(&mut s).await {
                    Ok(f) if f.kind == FRAME_LOCAL_DELIVER => n += 1,
                    Ok(_) => continue,
                    Err(e) => {
                        eprintln!("listener-side error: {e}");
                        break;
                    }
                }
            }
            let elapsed = start.elapsed();
            Ok::<_, anyhow::Error>((n, elapsed))
        }))
    } else {
        None
    };

    // Pequeño sleep para que el listener tenga tiempo de conectar al
    // QKC-3 y registrarse antes de que empiecen a llegar deliveries.
    if listener_handle.is_some() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Reusa una sola conexión al sender QKC para no abrir N veces.
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    let payload = vec![0xabu8; bytes];

    let send_start = Instant::now();
    for i in 0..count {
        let frame = build_local_send_with_seq(dest, payload.clone(), i);
        write_frame(&mut s, &frame).await?;
    }
    // Asegura flush.
    use tokio::io::AsyncWriteExt;
    s.flush().await?;
    // Cierre limpio: shutdown del write half → FIN al QKC. Sin esto,
    // dropear el TcpStream con datos sin consumir en el peer puede
    // generar RST (Linux), y el QKC perdería los frames pendientes
    // de leer en el receive buffer. Después esperamos a leer EOF (0
    // bytes) — eso confirma que el QKC ha drenado todo el buffer y
    // cerrado su half. Solo entonces dropeamos el socket.
    s.shutdown().await?;
    let mut buf = [0u8; 64];
    loop {
        use tokio::io::AsyncReadExt;
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let send_elapsed = send_start.elapsed();
    let send_secs = send_elapsed.as_secs_f64();
    println!(
        "── send side ──\n  frames: {count}\n  bytes/frame: {bytes}\n  send wall-time: {:.3}s\n  send-side throughput: {:.0} fps  ({:.2} MB/s payload)",
        send_secs,
        count as f64 / send_secs,
        (count as f64 * bytes as f64) / send_secs / 1_048_576.0,
    );

    if let Some(h) = listener_handle {
        match h.await? {
            Ok((n, elapsed)) => {
                let s = elapsed.as_secs_f64();
                println!(
                    "── end-to-end ──\n  delivered: {n}/{count}\n  wall-time: {:.3}s\n  e2e throughput: {:.0} fps  ({:.2} MB/s payload)",
                    s,
                    n as f64 / s,
                    (n as f64 * bytes as f64) / s / 1_048_576.0,
                );
            }
            Err(e) => eprintln!("listener task error: {e}"),
        }
    }

    Ok(())
}

fn build_local_send(dest: u32, payload: Vec<u8>) -> Frame {
    let mut f = Frame::empty(FRAME_LOCAL_SEND);
    f.sender_id = 0; // ORR no tiene id propio
    f.receiver_id = 0;
    f.dest_final = dest;
    f.key_size_bits = 0; // sin cifrar
    f.header_mp = vec![0x80]; // msgpack: mapa vacío
    f.payload = payload;
    f
}

fn build_local_send_with_seq(dest: u32, mut payload: Vec<u8>, seq: u64) -> Frame {
    // Empotra el número de secuencia en los primeros 8 bytes del
    // payload (si caben) para que el listener pueda detectar
    // reordering / pérdidas si quisiera.
    if payload.len() >= 8 {
        payload[..8].copy_from_slice(&seq.to_le_bytes());
    }
    build_local_send(dest, payload)
}
