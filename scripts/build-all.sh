#!/usr/bin/env bash
# Build every binary in release mode.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --workspace
echo "Binaries available under target/release/:"
ls -1 target/release | grep -E '^(qkc|orr|sdn|dkms|quditto)$' || true
