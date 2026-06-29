//! Microbenchmark del generador: cuántas claves/s puede crear quditto
//! en un solo thread, sin pasar por el FIFO ni por HTTP.
//!
//! Uso:
//!
//! ```text
//!   cargo run --release -p quditto --bin quditto-bench -- [--seconds N]
//! ```
//!
//! Reporta keys/s, bytes/s (entropy throughput) y latencia media por
//! mint. Como referencia: en hardware moderno de servidor con
//! `ChaCha20Rng` userspace esperamos > 10M keys/s en un solo thread.

use std::time::{Duration, Instant};

use clap::Parser;
use quditto::crypto::{build_rng, Key};

#[derive(Parser, Debug)]
#[command(name = "quditto-bench", about = "Microbenchmark for the mint loop")]
struct Args {
    /// Duración del benchmark en segundos.
    #[arg(long, default_value_t = 3.0)]
    seconds: f64,

    /// Volcar también un sample del primer batch (para verificar
    /// visualmente que la salida parece aleatoria).
    #[arg(long, default_value_t = false)]
    sample: bool,
}

fn main() {
    let args = Args::parse();
    let mut rng = build_rng();

    if args.sample {
        let k = Key::mint(&mut rng, 32);
        println!("sample key_id = {}", k.key_id);
        println!("sample material[..8] = {:02x?}", &k.material[..8]);
    }

    // Warmup — saca el primer batch del coste de cold cache.
    for _ in 0..10_000 {
        let _ = Key::mint(&mut rng, 32);
    }

    let target = Duration::from_secs_f64(args.seconds);
    let start = Instant::now();
    let mut n: u64 = 0;
    // Sumideros para que el compilador no elimine el cómputo.
    let mut id_xor: u128 = 0;
    let mut byte_xor: u8 = 0;

    while start.elapsed() < target {
        // Lotes de 8192 para amortizar el coste del check de tiempo.
        for _ in 0..8192 {
            let k = Key::mint(&mut rng, 32);
            id_xor ^= k.key_id.as_u128();
            byte_xor ^= k.material[0];
        }
        n += 8192;
    }

    let elapsed = start.elapsed().as_secs_f64();
    let keys_per_s = n as f64 / elapsed;
    let bytes_per_s = keys_per_s * 32.0; // solo el material
    let ns_per_key = elapsed * 1e9 / n as f64;

    println!("─── quditto mint benchmark ───");
    println!("elapsed:           {elapsed:.3} s");
    println!("keys minted:       {n}");
    println!("keys/s:            {keys_per_s:>12.0}");
    println!("material MB/s:     {:>12.2}", bytes_per_s / 1_048_576.0);
    println!("ns per mint:       {ns_per_key:>12.1}");
    // Imprime los sumideros para que LTO no los borre.
    std::hint::black_box(id_xor);
    std::hint::black_box(byte_xor);
}
