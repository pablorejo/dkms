#!/usr/bin/env bash
# Apaga todo lo que arrancó start.sh + barrida defensiva por nombre,
# por si los PIDs se hubieran perdido.
set -u
LOGS=/tmp/dkms-rust-demo
PIDS=$LOGS/pids

if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    while read -r pid name; do
        if kill -0 "$pid" 2>/dev/null; then
            echo "  ✕ $name (pid $pid)"
            kill "$pid" 2>/dev/null || true
        fi
    done < "$PIDS"
fi
sleep 0.3

# Barrida por nombre del binario (limitada a procesos del usuario
# actual para no tocar nada ajeno).
for proc in quditto qkc qkc-test-client; do
    pgrep -u "$(id -u)" -x "$proc" 2>/dev/null | while read -r p; do
        echo "  ✕ barrida: $proc (pid $p)"
        kill "$p" 2>/dev/null || true
    done
done
sleep 0.2
for proc in quditto qkc qkc-test-client; do
    pgrep -u "$(id -u)" -x "$proc" 2>/dev/null | while read -r p; do
        kill -9 "$p" 2>/dev/null || true
    done
done

: > "$PIDS" 2>/dev/null || true
echo "✓ parados."
