// Compiles every .proto under ../proto into Rust modules under
// `common::proto::*`. Re-run is triggered when any .proto file changes.

use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from("../proto");

    let protos: Vec<PathBuf> = fs::read_dir(&proto_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("proto"))
        .collect();

    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed=../proto");

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&out_dir)
        .compile_protos(&protos, &[proto_dir])?;

    Ok(())
}
