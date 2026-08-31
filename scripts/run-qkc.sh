#!/usr/bin/env bash
# El QKC no usa CONFIG_DIR: carga exactamente `--config <path>`. Sin
# argumentos se usa la config de desarrollo qkc/config/default.toml.
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}"
[ $# -eq 0 ] && set -- --config "$PWD/qkc/config/default.toml"
cargo run --release -p qkc -- "$@"
