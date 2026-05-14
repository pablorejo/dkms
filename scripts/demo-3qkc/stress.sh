#!/usr/bin/env bash
# Stress test end-to-end: manda N frames desde QKC-1 con dest=3 y mide
# tanto el send-side throughput como el e2e (envío→entrega en QKC-3).
#
# Uso:
#   ./stress.sh                 # 1000 frames de 64 B
#   ./stress.sh 10000 64        # 10000 frames de 64 B
#   ./stress.sh 1000 4096       # 1000 frames de 4 KiB
set -euo pipefail
COUNT=${1:-1000}
BYTES=${2:-64}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CLIENT="$ROOT/target/release/qkc-test-client"

echo "stress: count=$COUNT bytes=$BYTES  path: 1 → 2 → 3"
"$CLIENT" stress \
    --addr 127.0.0.1:7101 \
    --dest 3 \
    --count "$COUNT" \
    --bytes "$BYTES" \
    --wait-deliver-on 127.0.0.1:7103
