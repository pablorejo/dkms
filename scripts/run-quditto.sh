#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CONFIG_DIR="${CONFIG_DIR:-$PWD/quditto/config}"
export RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}"
cargo run --release -p quditto -- "$@"
