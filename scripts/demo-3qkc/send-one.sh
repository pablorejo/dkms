#!/usr/bin/env bash
# Lanza un listener en QKC-3 y manda UN mensaje desde QKC-1 con dest=3.
# El listener imprime la entrega y se cierra al recibir el primero.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CLIENT="$ROOT/target/release/qkc-test-client"

# Listener en background, sale tras recibir 1.
"$CLIENT" listen --addr 127.0.0.1:7103 --count 1 --verbose &
LISTEN_PID=$!

# Dale tiempo a conectarse al QKC-3.
sleep 0.2

"$CLIENT" send --addr 127.0.0.1:7101 --dest 3 --message "hello-end-to-end"

wait "$LISTEN_PID"
