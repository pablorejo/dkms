//! Genera las identidades de **firma ML-DSA** de una malla de pruebas.
//!
//! El handshake PQC del QKC (`pqc_auth = sign`) y el bootstrap del ORR firman
//! con una identidad ML-DSA cuya semilla vive en la config del nodo y cuya
//! clave pública se reparte a sus pares. A diferencia de los certificados, esto
//! no lo puede hacer openssl: la semilla es el formato de la crate `ml-dsa`, y
//! el consumidor es el propio runtime. De ahí esta herramienta.
//!
//!     pqc_keygen --out DIR --nodes 30
//!
//! Deja, por cada nodo n:
//!     DIR/qkc-<n>.seed   DIR/qkc-<n>.vk      (semilla y clave de verificación)
//!     DIR/orr_<n>.seed   DIR/orr_<n>.vk
//! todo en base64, que es como lo consumen los `node.yml`.
//!
//! Las semillas son material secreto: quedan en el directorio de la malla, que
//! es efímero (scratch del job), y nunca se anuncian — solo viajan las `.vk`.

#![forbid(unsafe_code)]
use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "pqc_keygen",
    about = "Identidades de firma ML-DSA para la malla"
)]
struct Args {
    /// Directorio de salida (se crea si no existe).
    #[arg(long)]
    out: PathBuf,
    /// Número de nodos: se generan identidades para 1..=N.
    #[arg(long)]
    nodes: u32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    fs::create_dir_all(&args.out).with_context(|| format!("creando {}", args.out.display()))?;
    let b64 = base64::engine::general_purpose::STANDARD;

    for n in 1..=args.nodes {
        // QKC y ORR son procesos distintos con identidades distintas: si
        // compartieran clave, comprometer uno comprometería al otro.
        for who in [format!("qkc-{n}"), format!("orr_{n}")] {
            let kp = common::crypto::pqc_sign::keygen();
            fs::write(
                args.out.join(format!("{who}.seed")),
                b64.encode(kp.secret_seed.as_slice()),
            )?;
            fs::write(
                args.out.join(format!("{who}.vk")),
                b64.encode(&kp.verifying_key),
            )?;
        }
    }
    println!(
        "pqc_keygen: {} identidades ML-DSA en {} ({} nodos x qkc+orr)",
        args.nodes * 2,
        args.out.display(),
        args.nodes
    );
    Ok(())
}
