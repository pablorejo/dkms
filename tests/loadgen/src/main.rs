//! Cliente SAE de carga — equivalente Rust de `tests/testbed/sae_load.py`.
//!
//! Existe por una razón concreta: el `ssl` de Python usa el OpenSSL del
//! sistema, y en CESGA es **1.1.1g (2020)**, que no conoce ML-DSA. Con certs
//! post-cuánticos el cliente Python ni siquiera carga el par
//! (`load_cert_chain` falla), así que no se puede medir el arm PQC con él.
//! Este cliente usa el mismo `common::tls_pqc` que los módulos, así que habla
//! mTLS con certificados **RSA y ML-DSA** indistintamente — que es justo la
//! comparación que la campaña quiere hacer.
//!
//! Interfaz y salida son **idénticas** a las del script Python para que
//! `scale_one.sh` y el análisis existente no cambien:
//!
//! ```text
//! t_unix,thread,status,latency_ms,n_keys,key_id,slave
//! ```
//!
//! `status` es el código HTTP, o `ERR:<tipo>` si la petición no llegó a
//! responder. Con `--aggregate-throttled` los 429/503 se cuentan por segundo
//! y se emite una fila resumen con `thread=-1` (bajo saturación, una fila por
//! petición rechazada convierte el CSV en cientos de MB).
//!
//! Cada hilo mantiene su propia conexión keep-alive contra un destino fijo
//! (hilo i → esclavo i mod N), como el original: así la medida es la cadena
//! DKMS→ORR→QKC y no el coste de reconectar.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::Parser;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "sae_load", about = "Cliente SAE de carga (mTLS, RSA o ML-DSA)")]
struct Args {
    /// Id del SAE maestro (el del cert de cliente que se presenta). No hace
    /// falta en `--roundtrip`, que recorre los pares por su cuenta.
    #[arg(long, default_value = "")]
    sae: String,
    /// Modo verificación: por cada par ORDENADO de nodos, el maestro pide
    /// `enc_keys` en su DKMS, el esclavo recupera esa `key_ID` con `dec_keys`
    /// en el suyo y se comparan los BYTES. Es el equivalente de
    /// `keys_smoke.sh`, que usa `curl` y por tanto el OpenSSL del sistema:
    /// con certs ML-DSA aquel aborta el handshake y no mide nada.
    #[arg(long, default_value_t = false)]
    roundtrip: bool,
    /// Nodos a recorrer en `--roundtrip`, separados por comas (p. ej. "1,5,9").
    #[arg(long, default_value = "")]
    nodes: String,
    /// Id del SAE destino (alternativa a `--slaves`).
    #[arg(long, default_value = "")]
    slave: String,
    /// Lista de destinos separada por comas; el hilo i va al esclavo i mod N.
    #[arg(long, default_value = "")]
    slaves: String,
    /// Directorio con `net-ca.crt` y `<sae>.crt` / `<sae>.key`.
    #[arg(long, default_value = "/home/debian/site/certs")]
    certs: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 20005)]
    port: u16,
    #[arg(long, default_value_t = 1)]
    threads: usize,
    #[arg(long, default_value_t = 60.0)]
    duration: f64,
    /// Claves por petición.
    #[arg(long, default_value_t = 1)]
    number: u32,
    /// Bits por clave.
    #[arg(long, default_value_t = 256)]
    size: u32,
    /// Límite de peticiones/s por hilo (0 = sin límite).
    #[arg(long = "rate-cap", default_value_t = 0.0)]
    rate_cap: f64,
    #[arg(long, default_value_t = 20.0)]
    timeout: f64,
    /// CSV de salida (obligatorio salvo en `--roundtrip`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Fichero `key_id,sha256` para la verificación por muestreo.
    #[arg(long = "record-keys", default_value = "")]
    record_keys: String,
    /// Cuenta los 429/503 por segundo en vez de una fila por petición.
    #[arg(long = "aggregate-throttled", default_value_t = false)]
    aggregate_throttled: bool,
}

/// Fila lista para escribir, o una clave a registrar para la verificación.
enum Out {
    Row(String),
    Key { slave: String, line: String },
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Cliente HTTPS con mTLS. El provider PQC ya está instalado como default del
/// proceso, así que `Identity::from_pem` acepta claves ML-DSA además de las
/// clásicas. Un cliente por hilo con el pool a 1 ⇒ una conexión keep-alive por
/// hilo, como el original Python.
fn build_client(args: &Args) -> Result<reqwest::Client> {
    build_client_as(args, &args.sae)
}

/// Igual que [`build_client`] pero presentando la identidad indicada: en
/// `--roundtrip` cada lado del par usa su propio cert de SAE.
fn build_client_as(args: &Args, sae: &str) -> Result<reqwest::Client> {
    let ca_path = args.certs.join("net-ca.crt");
    let cert_path = args.certs.join(format!("{}.crt", sae));
    let key_path = args.certs.join(format!("{}.key", sae));

    let mut bundle = std::fs::read(&cert_path)
        .with_context(|| format!("leyendo cert de cliente {}", cert_path.display()))?;
    if !bundle.ends_with(b"\n") {
        bundle.push(b'\n');
    }
    bundle.extend_from_slice(
        &std::fs::read(&key_path)
            .with_context(|| format!("leyendo clave de cliente {}", key_path.display()))?,
    );
    let identity = reqwest::Identity::from_pem(&bundle)
        .context("Identity::from_pem (¿clave no soportada por el provider?)")?;

    let mut b = reqwest::Client::builder()
        .use_rustls_tls()
        .identity(identity)
        .https_only(true)
        .pool_max_idle_per_host(1)
        .tcp_keepalive(Duration::from_secs(30))
        .timeout(Duration::from_secs_f64(args.timeout));
    for ca in split_pem(
        &std::fs::read(&ca_path).with_context(|| format!("leyendo CA {}", ca_path.display()))?,
    )? {
        b = b.add_root_certificate(ca);
    }
    b.build().context("construyendo el cliente HTTPS")
}

/// reqwest no acepta multi-PEM en un `Certificate`: separamos el bundle.
fn split_pem(pem: &[u8]) -> Result<Vec<reqwest::Certificate>> {
    let text = std::str::from_utf8(pem).context("CA no es UTF-8")?;
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        cur.push_str(line);
        cur.push('\n');
        if line.starts_with("-----END CERTIFICATE-----") {
            out.push(reqwest::Certificate::from_pem(cur.as_bytes())?);
            cur.clear();
        }
    }
    anyhow::ensure!(!out.is_empty(), "la CA no tiene bloques CERTIFICATE");
    Ok(out)
}

/// Clasifica un error de reqwest en la etiqueta `ERR:<tipo>` del CSV. Los
/// nombres son los que se buscan al analizar: distinguen "no conecté" (DKMS
/// caído) de "el TLS falló" (certificado) de "expiró" (saturación).
fn err_tag(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "ERR:timeout"
    } else if e.is_connect() {
        // Incluye los fallos de handshake TLS: en el arm de certs, si esto
        // aparece de golpe en todos los hilos, mira el `tls handshake failed`
        // del DKMS — es un problema de cadena/CA, no de carga.
        "ERR:connect"
    } else if e.is_decode() {
        "ERR:badjson"
    } else if e.is_body() || e.is_request() {
        "ERR:request"
    } else {
        "ERR:other"
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker(
    idx: usize,
    args: Arc<Args>,
    slave: String,
    client: reqwest::Client,
    deadline: Instant,
    tx: mpsc::UnboundedSender<Out>,
    ok_counter: Arc<AtomicU64>,
) {
    let url = format!(
        "https://{}:{}/api/v1/keys/{}/enc_keys",
        args.host, args.port, slave
    );
    let body = serde_json::json!({ "number": args.number, "size": args.size });
    let min_period = if args.rate_cap > 0.0 {
        Some(Duration::from_secs_f64(1.0 / args.rate_cap))
    } else {
        None
    };
    // Acumulador de 429/503 del segundo en curso (--aggregate-throttled).
    let mut throttled: Vec<(String, u64)> = Vec::new();
    let mut throttled_second = now_unix() as u64;

    while Instant::now() < deadline {
        let cycle = Instant::now();
        let t0 = Instant::now();
        let resp = client.post(&url).json(&body).send().await;

        let (status, n_keys, key_id, dt) = match resp {
            Ok(r) => {
                let code = r.status().as_u16();
                let dt = t0.elapsed().as_secs_f64() * 1000.0;
                if code == 200 {
                    match r.json::<serde_json::Value>().await {
                        Ok(doc) => {
                            let ks = doc
                                .get("keys")
                                .and_then(|k| k.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let first = ks
                                .first()
                                .and_then(|k| k.get("key_ID"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !args.record_keys.is_empty() {
                                for k in &ks {
                                    // sha256 del string base64 tal cual, como
                                    // el cliente Python: el verificador de
                                    // stress.sh compara contra ese digest.
                                    let kb = k.get("key").and_then(|v| v.as_str()).unwrap_or("");
                                    let kid =
                                        k.get("key_ID").and_then(|v| v.as_str()).unwrap_or("");
                                    let digest = hex::encode(Sha256::digest(kb.as_bytes()));
                                    let _ = tx.send(Out::Key {
                                        slave: slave.clone(),
                                        line: format!("{kid},{digest}"),
                                    });
                                }
                            }
                            ok_counter.fetch_add(1, Ordering::Relaxed);
                            (code.to_string(), ks.len(), first, dt)
                        }
                        Err(_) => ("ERR:badjson".to_string(), 0, String::new(), dt),
                    }
                } else {
                    (code.to_string(), 0, String::new(), dt)
                }
            }
            Err(e) => {
                let dt = t0.elapsed().as_secs_f64() * 1000.0;
                // Sin esta pausa, un DKMS caído convierte el test en un bucle
                // ocupado que mide la velocidad del bucle, no la del sistema.
                tokio::time::sleep(Duration::from_millis(50)).await;
                (err_tag(&e).to_string(), 0, String::new(), dt)
            }
        };

        if args.aggregate_throttled && (status == "429" || status == "503") {
            match throttled.iter_mut().find(|(s, _)| *s == status) {
                Some((_, n)) => *n += 1,
                None => throttled.push((status.clone(), 1)),
            }
            let now_s = now_unix() as u64;
            if now_s != throttled_second {
                for (st, n) in throttled.drain(..) {
                    let _ = tx.send(Out::Row(format!(
                        "{throttled_second}.000,-1,{st},0.00,0,x{n},{slave}"
                    )));
                }
                throttled_second = now_s;
            }
        } else {
            let _ = tx.send(Out::Row(format!(
                "{:.3},{idx},{status},{dt:.2},{n_keys},{key_id},{slave}",
                now_unix()
            )));
        }

        if let Some(p) = min_period {
            let spent = cycle.elapsed();
            if spent < p {
                tokio::time::sleep(p - spent).await;
            }
        }
    }
    // Vuelca lo que quede del último segundo agregado.
    for (st, n) in throttled.drain(..) {
        let _ = tx.send(Out::Row(format!(
            "{throttled_second}.000,-1,{st},0.00,0,x{n},{slave}"
        )));
    }
}

/// Puerto del plano SAE del nodo `n` en la malla de `mesh.sh`.
fn sae_port(n: u32) -> u16 {
    20005 + ((n - 1) as u16) * 100
}

/// Un intercambio ETSI-014 completo por cada par ORDENADO: el maestro pide
/// `enc_keys` en su DKMS y el esclavo recupera esa `key_ID` con `dec_keys` en
/// el suyo; se comparan los BYTES.
///
/// La comparación es el punto: la clave de sesión viaja envuelta en OTP con una
/// clave de transporte y **no lleva integridad propia**, así que si el material
/// estuviera desalineado los dos SAE se llevarían claves distintas sin que nada
/// fallase. Pares ordenados porque el `buffer_enc` de un extremo es el
/// `buffer_dec` del otro: A→B y B→A gastan material distinto.
async fn run_roundtrip(args: &Args) -> Result<i32> {
    let nodes: Vec<u32> = args
        .nodes
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().parse::<u32>())
        .collect::<std::result::Result<_, _>>()
        .context("--nodes debe ser una lista de enteros separados por comas")?;
    anyhow::ensure!(nodes.len() >= 2, "--roundtrip necesita al menos 2 nodos");

    // Un cliente por identidad de SAE, reutilizado en todos sus pares.
    let mut clients = std::collections::HashMap::new();
    for n in &nodes {
        clients.insert(*n, build_client_as(args, &format!("sae_{n}"))?);
    }

    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut fails: Vec<String> = Vec::new();

    for m in &nodes {
        for s in &nodes {
            if m == s {
                continue;
            }
            let enc_url = format!(
                "https://{}:{}/api/v1/keys/sae_{s}/enc_keys",
                args.host,
                sae_port(*m)
            );
            let enc: serde_json::Value = match clients[m]
                .post(&enc_url)
                .json(&serde_json::json!({"number": 1, "size": 256}))
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        fails.push(format!("sae_{m}→sae_{s} enc_keys json: {e}"));
                        bad += 1;
                        continue;
                    }
                },
                Err(e) => {
                    fails.push(format!("sae_{m}→sae_{s} enc_keys: {e}"));
                    bad += 1;
                    continue;
                }
            };
            let kid = enc["keys"][0]["key_ID"].as_str().unwrap_or("").to_string();
            let master_key = enc["keys"][0]["key"].as_str().unwrap_or("").to_string();
            if kid.is_empty() || master_key.is_empty() {
                fails.push(format!("sae_{m}→sae_{s} enc_keys sin clave"));
                bad += 1;
                continue;
            }

            let dec_url = format!(
                "https://{}:{}/api/v1/keys/sae_{m}/dec_keys",
                args.host,
                sae_port(*s)
            );
            let dec: serde_json::Value = match clients[s]
                .post(&dec_url)
                .json(&serde_json::json!({"key_IDs": [{"key_ID": kid}]}))
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        fails.push(format!("sae_{m}→sae_{s} dec_keys json: {e}"));
                        bad += 1;
                        continue;
                    }
                },
                Err(e) => {
                    fails.push(format!("sae_{m}→sae_{s} dec_keys: {e}"));
                    bad += 1;
                    continue;
                }
            };
            let slave_key = dec["keys"][0]["key"].as_str().unwrap_or("").to_string();
            if slave_key.is_empty() {
                fails.push(format!("sae_{m}→sae_{s} dec_keys sin clave"));
                bad += 1;
            } else if slave_key == master_key {
                ok += 1;
            } else {
                fails.push(format!(
                    "sae_{m}→sae_{s} LOS BYTES NO COINCIDEN key_ID={kid}"
                ));
                bad += 1;
            }
        }
    }

    let n = nodes.len();
    println!(
        "  pares ordenados: {}   idénticos: {ok}   fallidos: {bad}",
        n * (n - 1)
    );
    for f in &fails {
        println!("    ✗ {f}");
    }
    Ok(if bad == 0 { 0 } else { 1 })
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Provider con ML-DSA + clásicos: es lo que permite presentar un cert de
    // cliente post-cuántico. Sin esto, `Identity::from_pem` con una clave
    // ML-DSA falla con "failed to parse private key as RSA, ECDSA, or EdDSA".
    common::tls_pqc::ensure_process_default().map_err(anyhow::Error::msg)?;

    if args.roundtrip {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let code = rt.block_on(run_roundtrip(&args))?;
        std::process::exit(code);
    }

    let out_path = args
        .out
        .clone()
        .context("--out es obligatorio sin --roundtrip")?;
    anyhow::ensure!(!args.sae.is_empty(), "--sae es obligatorio sin --roundtrip");
    let slave_list: Vec<String> = if !args.slaves.is_empty() {
        args.slaves
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    } else if !args.slave.is_empty() {
        vec![args.slave.clone()]
    } else {
        anyhow::bail!("hace falta --slave o --slaves");
    };

    // Pocos worker threads por proceso: la campaña levanta un proceso por
    // maestro (N=30 ⇒ 30 procesos). Un runtime con `nproc` hilos cada uno
    // ahogaría el nodo y mediría el generador de carga, no el sistema.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let args = Arc::new(args);
        let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
        let deadline = Instant::now() + Duration::from_secs_f64(args.duration);
        let ok_counter = Arc::new(AtomicU64::new(0));

        let mut tasks = Vec::with_capacity(args.threads);
        for i in 0..args.threads {
            // Un cliente por hilo ⇒ una conexión keep-alive por hilo.
            let client = build_client(&args)?;
            let slave = slave_list[i % slave_list.len()].clone();
            tasks.push(tokio::spawn(worker(
                i,
                args.clone(),
                slave,
                client,
                deadline,
                tx.clone(),
                ok_counter.clone(),
            )));
        }
        drop(tx); // el writer termina cuando todos los workers sueltan su tx

        // Escritor único: el CSV se escribe en un sitio, sin contención.
        let out =
            File::create(&out_path).with_context(|| format!("creando {}", out_path.display()))?;
        let mut csv = BufWriter::new(out);
        writeln!(csv, "t_unix,thread,status,latency_ms,n_keys,key_id,slave")?;
        let mut keyfiles: Vec<(String, BufWriter<File>)> = Vec::new();
        let split_keys = slave_list.len() > 1;

        while let Some(item) = rx.recv().await {
            match item {
                Out::Row(r) => writeln!(csv, "{r}")?,
                Out::Key { slave, line } => {
                    let name = if split_keys {
                        format!("{}.{}.keys", args.record_keys, slave)
                    } else {
                        args.record_keys.clone()
                    };
                    let pos = match keyfiles.iter().position(|(n, _)| *n == name) {
                        Some(p) => p,
                        None => {
                            let fh = BufWriter::new(File::create(Path::new(&name))?);
                            keyfiles.push((name, fh));
                            keyfiles.len() - 1
                        }
                    };
                    writeln!(keyfiles[pos].1, "{line}")?;
                }
            }
        }
        for t in tasks {
            let _ = t.await;
        }
        csv.flush()?;
        for (_, mut f) in keyfiles {
            f.flush()?;
        }
        // Una línea a stderr con el veredicto: `check_load` mira los .err, y
        // esto distingue "arrancó y sirvió" de "arrancó y falló todo".
        eprintln!(
            "sae_load {}: {} respuestas 200 en {:.0}s ({} hilos, {} destinos)",
            args.sae,
            ok_counter.load(Ordering::Relaxed),
            args.duration,
            args.threads,
            slave_list.len()
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}
