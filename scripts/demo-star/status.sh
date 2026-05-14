#!/usr/bin/env bash
set -u
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/pids
echo "── procesos:"
[ -f "$PIDS" ] && while read -r pid name; do
    if kill -0 "$pid" 2>/dev/null; then
        echo "  ✓ $name (pid $pid)"
    else
        echo "  ✗ $name (pid $pid) MUERTO"
        tail -5 "$LOGS/$name.log" 2>/dev/null | sed 's/^/      /'
    fi
done < "$PIDS"

echo
echo "── healthz QKCs:"
for p in 7200 7201 7202 7203 7204 7211 7222 7233 7244; do
    code=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "http://127.0.0.1:$p/healthz" 2>/dev/null || echo 000)
    [ "$code" = "200" ] && echo "  ✓ qkc-admin :$p" || echo "  ✗ qkc-admin :$p ($code)"
done

echo
echo "── qudittos:"
for p in 8011 8012 8021 8022 8031 8032 8041 8042; do
    code=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "http://127.0.0.1:$p/healthz" 2>/dev/null || echo 000)
    [ "$code" = "200" ] && echo "  ✓ quditto :$p" || echo "  ✗ quditto :$p ($code)"
done

echo
echo "── forwarding tables:"
for p in 7200 7201 7202 7203 7204 7211 7222 7233 7244; do
    body=$(curl -s -m 2 "http://127.0.0.1:$p/forwarding-table" 2>/dev/null || echo '<no resp>')
    echo "  :$p  $body"
done
