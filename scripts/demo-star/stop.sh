#!/usr/bin/env bash
# Apaga el demo-star.
set -u
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/pids
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    while read -r pid name; do
        kill -0 "$pid" 2>/dev/null && { echo "  ✕ $name (pid $pid)"; kill "$pid" 2>/dev/null || true; }
    done < "$PIDS"
fi
sleep 0.3
# Barrida por nombre del binario.
for proc in quditto qkc qkc-test-client; do
    pgrep -u "$(id -u)" -x "$proc" 2>/dev/null | while read -r p; do
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
