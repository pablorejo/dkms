#!/usr/bin/env bash
# quditto no lee ficheros de config: va por flags CLI con fallbacks de env
# QUDITTO_* (--listen, --r0, --alpha, --distance, ...). Ejemplo:
#   QUDITTO_R0=2000 ./scripts/run-quditto.sh
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}"
cargo run --release -p quditto -- "$@"
