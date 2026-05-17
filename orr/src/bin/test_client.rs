//! `orr-test-client` — simula un DKMS hablando al ORR por gRPC.
//!
//! Dos subcomandos:
//!
//! ```text
//!   orr-test-client send   --addr http://127.0.0.1:50511 --dest ORR_44 --message "hola"
//!   orr-test-client listen --addr http://127.0.0.1:50544
//! ```
//!
//! `send` llama a `OrrControl::SendMessage` con `max_hops=0` (passthrough)
//! por defecto. `listen` se suscribe a `StreamDeliveries` e imprime cada
//! `DeliveredMessage` que recibe.

use std::collections::HashMap;

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use clap::{Parser, Subcommand};
use common::proto::{
    common::v1::NodeId,
    orr::v1::{
        orr_control_client::OrrControlClient, GetPublicKeyRequest, SendMessageRequest,
        StreamDeliveriesRequest,
    },
};
use std::time::Instant;
use tonic::transport::Channel;

#[derive(Parser, Debug)]
#[command(name = "orr-test-client", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Send {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        dest: String,
        #[arg(long, default_value = "hello")]
        message: String,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        max_hops: i32,
        /// key=value pairs para el app_header (dkms_header). Repetible.
        #[arg(long = "hdr")]
        headers: Vec<String>,
    },
    Listen {
        #[arg(long)]
        addr: String,
        #[arg(long, default_value = "test-client")]
        subscriber_id: String,
        #[arg(long)]
        count: Option<u64>,
    },
    /// Mete N mensajes lo más rápido posible vía SendMessage. Útil
    /// para tests de saturación por flujo en un modo dado.
    Stress {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        dest: String,
        #[arg(long, default_value_t = 1000)]
        count: u64,
        #[arg(long, default_value_t = 64)]
        bytes: usize,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        max_hops: i32,
        /// Si se da, los frames llevan `orr_path` en el app_header — necesario
        /// para `max_hops = -1` o `>=2`. CSV de orr_ids del path.
        #[arg(long, default_value = "")]
        orr_path: String,
    },
    /// Devuelve la pubkey ML-KEM (base64) del ORR. Útil para snapshots.
    GetPubkey {
        #[arg(long)]
        addr: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Send {
            addr,
            dest,
            message,
            max_hops,
            headers,
        } => do_send(&addr, &dest, message.into_bytes(), max_hops, headers).await,
        Cmd::Listen {
            addr,
            subscriber_id,
            count,
        } => do_listen(&addr, &subscriber_id, count).await,
        Cmd::Stress {
            addr,
            dest,
            count,
            bytes,
            max_hops,
            orr_path,
        } => do_stress(&addr, &dest, count, bytes, max_hops, &orr_path).await,
        Cmd::GetPubkey { addr } => do_get_pubkey(&addr).await,
    }
}

async fn do_stress(
    addr: &str,
    dest: &str,
    count: u64,
    bytes: usize,
    max_hops: i32,
    orr_path: &str,
) -> Result<()> {
    let ch = Channel::from_shared(addr.to_string())?.connect().await?;
    let mut client = OrrControlClient::new(ch);
    let payload_template = vec![0xabu8; bytes];

    let mut app_header: HashMap<String, String> = HashMap::new();
    if !orr_path.is_empty() {
        app_header.insert("orr_path".into(), orr_path.into());
    }

    let start = Instant::now();
    for i in 0..count {
        let mut payload = payload_template.clone();
        if payload.len() >= 8 {
            payload[..8].copy_from_slice(&i.to_le_bytes());
        }
        let resp = client
            .send_message(SendMessageRequest {
                destination: Some(NodeId {
                    value: dest.to_string(),
                }),
                payload,
                max_hops,
                has_max_hops: true,
                app_header: app_header.clone(),
                fire_and_forget: true,
            })
            .await;
        if let Err(s) = resp {
            eprintln!("send error at i={i}: {s}");
            return Err(s.into());
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "── stress send {dest} max_hops={max_hops}\n  frames: {count}\n  bytes/frame: {bytes}\n  wall-time: {:.3}s\n  send-side throughput: {:.0} fps  ({:.2} MB/s payload)",
        elapsed,
        count as f64 / elapsed.max(1e-9),
        (count as f64 * bytes as f64) / elapsed.max(1e-9) / 1_048_576.0,
    );
    Ok(())
}

async fn do_get_pubkey(addr: &str) -> Result<()> {
    let ch = Channel::from_shared(addr.to_string())?.connect().await?;
    let mut client = OrrControlClient::new(ch);
    let resp = client
        .get_public_key(GetPublicKeyRequest {})
        .await?
        .into_inner();
    let id = resp.orr_id.map(|n| n.value).unwrap_or_default();
    println!(
        "orr_id={} suite={} pubkey_len={}",
        id,
        resp.suite,
        resp.public_key.len()
    );
    println!("pubkey_b64={}", BASE64.encode(&resp.public_key));
    Ok(())
}

async fn do_send(
    addr: &str,
    dest: &str,
    payload: Vec<u8>,
    max_hops: i32,
    headers: Vec<String>,
) -> Result<()> {
    let mut app_header: HashMap<String, String> = HashMap::new();
    for h in &headers {
        let (k, v) = h.split_once('=').unwrap_or((h.as_str(), ""));
        app_header.insert(k.to_string(), v.to_string());
    }

    let ch = Channel::from_shared(addr.to_string())?.connect().await?;
    let mut client = OrrControlClient::new(ch);
    let resp = client
        .send_message(SendMessageRequest {
            destination: Some(NodeId {
                value: dest.to_string(),
            }),
            payload,
            max_hops,
            has_max_hops: true,
            app_header,
            fire_and_forget: false,
        })
        .await?
        .into_inner();
    println!(
        "OK status={:?} dest={:?} next_hop_qkc={} remaining_hops={} pqc_layer={}",
        resp.status,
        resp.final_destination.map(|n| n.value).unwrap_or_default(),
        resp.next_hop_qkc,
        resp.remaining_hops,
        resp.pqc_layer,
    );
    Ok(())
}

async fn do_listen(addr: &str, subscriber_id: &str, count: Option<u64>) -> Result<()> {
    let ch = Channel::from_shared(addr.to_string())?.connect().await?;
    let mut client = OrrControlClient::new(ch);
    let mut stream = client
        .stream_deliveries(StreamDeliveriesRequest {
            subscriber_id: subscriber_id.to_string(),
        })
        .await?
        .into_inner();
    println!("listening on {addr} as {subscriber_id}");
    let mut n: u64 = 0;
    while let Some(msg) = stream.message().await? {
        n += 1;
        let origin = msg.origin.as_ref().map(|n| n.value.as_str()).unwrap_or("?");
        let dest = msg
            .destination
            .as_ref()
            .map(|n| n.value.as_str())
            .unwrap_or("?");
        println!(
            "[{n}] origin={} dest={} pqc={} bytes={} payload={}",
            origin,
            dest,
            msg.pqc_decapsulated,
            msg.payload.len(),
            String::from_utf8_lossy(&msg.payload),
        );
        if !msg.app_header.is_empty() {
            for (k, v) in &msg.app_header {
                println!("    {k} = {v}");
            }
        }
        if let Some(c) = count {
            if n >= c {
                break;
            }
        }
    }
    Ok(())
}
