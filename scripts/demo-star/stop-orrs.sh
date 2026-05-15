#!/usr/bin/env bash
set -u
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/orr-pids
if [ -f "$PIDS" ]; then
    while read -r pid name; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" && echo "  ✗ killed $name (pid $pid)"
        fi
    done < "$PIDS"
    : > "$PIDS"
fi
echo "── ORRs detenidos (QKC + quditto siguen corriendo)"
