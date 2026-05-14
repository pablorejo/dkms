#!/usr/bin/env bash
# Diagnóstico rápido: qué procesos están vivos y qué puertos responden.
set -u
LOGS=/tmp/dkms-rust-demo
PIDS=$LOGS/pids

echo "── procesos:"
if [ -f "$PIDS" ]; then
    while read -r pid name; do
        if kill -0 "$pid" 2>/dev/null; then
            echo "  ✓ $name (pid $pid)"
        else
            echo "  ✗ $name (pid $pid) — MUERTO"
            if [ -f "$LOGS/$name.log" ]; then
                echo "    últimas líneas del log:"
                tail -10 "$LOGS/$name.log" | sed 's/^/      /'
            fi
        fi
    done < "$PIDS"
else
    echo "  (sin $PIDS — usa ./start.sh)"
fi

echo
echo "── puertos:"
check_tcp() {
    local host=$1 port=$2 label=$3
    if (echo > "/dev/tcp/$host/$port") 2>/dev/null; then
        echo "  ✓ $label tcp://$host:$port"
    else
        echo "  ✗ $label tcp://$host:$port  (refused)"
    fi
}
check_http() {
    local url=$1 label=$2
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "$url" 2>/dev/null || echo 000)
    if [ "$code" = "200" ]; then
        echo "  ✓ $label http $url"
    else
        echo "  ✗ $label http $url  (HTTP $code)"
    fi
}

check_http "http://127.0.0.1:8081/healthz"           "quditto-A"
check_http "http://127.0.0.1:8082/healthz"           "quditto-B"
check_http "http://127.0.0.1:7201/healthz"           "qkc1-admin"
check_http "http://127.0.0.1:7202/healthz"           "qkc2-admin"
check_http "http://127.0.0.1:7203/healthz"           "qkc3-admin"
check_tcp  127.0.0.1 7001 "qkc1-peer"
check_tcp  127.0.0.1 7002 "qkc2-peer"
check_tcp  127.0.0.1 7003 "qkc3-peer"
check_tcp  127.0.0.1 7101 "qkc1-local"
check_tcp  127.0.0.1 7102 "qkc2-local"
check_tcp  127.0.0.1 7103 "qkc3-local"

echo
echo "── forwarding tables:"
for p in 7201 7202 7203; do
    body=$(curl -s -m 2 "http://127.0.0.1:$p/forwarding-table" 2>/dev/null || echo '<no resp>')
    echo "  :$p  $body"
done

echo
echo "── quditto status:"
for p in 8081 8082; do
    body=$(curl -s -m 2 "http://127.0.0.1:$p/api/v1/keys/qkc/status" 2>/dev/null || echo '<no resp>')
    echo "  :$p  $body"
done
