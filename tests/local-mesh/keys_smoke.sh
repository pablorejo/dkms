#!/usr/bin/env bash
# Un intercambio ETSI-014 completo por cada par ORDENADO de DKMS.
#
# El SAE maestro pide `enc_keys` en SU DKMS, el esclavo recupera esa `key_ID`
# con `dec_keys` en el SUYO, y se comparan los BYTES. La comparación es el
# punto: es lo único que prueba de verdad el camino, porque la clave de sesión
# viaja envuelta en OTP con una clave de transporte y **no lleva comprobación
# de integridad propia** — si el material de transporte estuviera desalineado,
# los dos SAE se llevarían claves distintas sin que nada fallase.
#
# Pares ordenados y no desordenados: la dirección importa. El `buffer_enc` de
# un extremo es el `buffer_dec` del otro, así que A→B y B→A gastan material
# distinto y pueden fallar por separado.
#
#   keys_smoke.sh [nodos...]     default: los que haya levantados
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="${DKMS_MESH_DIR:-$REPO/tests/results/local-mesh}"
C="$DIR/certs"
[ -d "$C" ] || { echo "keys_smoke: no hay malla en $DIR (mesh.sh up N)" >&2; exit 2; }

NODES=("$@")
if (( ${#NODES[@]} == 0 )); then
    total=$(cat "$DIR/N" 2>/dev/null || echo 0)
    (( total > 0 )) || { echo "keys_smoke: no sé cuántos nodos hay" >&2; exit 2; }
    mapfile -t NODES < <(seq 1 "$total")
fi

sae_port() { echo $(( 20005 + ($1 - 1) * 100 )); }

# Con certificados ML-DSA, `curl` no sirve: va contra el OpenSSL del sistema y
# los de antes de la 3.5 —CESGA tiene 1.1.1g— abortan el handshake (el DKMS lo
# ve como `tls handshake eof`). Medido el 2026-08-27: el régimen `poca` de la
# campaña dio 3960 handshakes fallidos y CERO claves, que parecía un fallo del
# DKMS y era del cliente. El binario Rust usa common::tls_pqc y habla con
# ambos, así que se prefiere cuando está.
LOADER_BIN="$REPO/target/release/sae_load"
if [ -x "$LOADER_BIN" ]; then
    exec "$LOADER_BIN" --roundtrip --certs "$C" \
        --nodes "$(IFS=,; echo "${NODES[*]}")"
fi

ok=0; bad=0; fails=()
for m in "${NODES[@]}"; do
  for s in "${NODES[@]}"; do
    [ "$m" = "$s" ] && continue

    enc=$(curl -sS --max-time 25 --cacert "$C/net-ca.crt" \
          --cert "$C/sae_$m.crt" --key "$C/sae_$m.key" \
          -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
          "https://127.0.0.1:$(sae_port "$m")/api/v1/keys/sae_$s/enc_keys" 2>&1)
    kid=$(jq -r '.keys[0].key_ID // empty' <<<"$enc" 2>/dev/null)
    master_key=$(jq -r '.keys[0].key // empty' <<<"$enc" 2>/dev/null)
    if [ -z "$kid" ] || [ -z "$master_key" ]; then
        fails+=("sae_$m→sae_$s enc_keys: $(head -c 100 <<<"$enc" | tr -d '\n')")
        bad=$((bad+1)); continue
    fi

    dec=$(curl -sS --max-time 25 --cacert "$C/net-ca.crt" \
          --cert "$C/sae_$s.crt" --key "$C/sae_$s.key" \
          -H 'Content-Type: application/json' \
          -d "{\"key_IDs\":[{\"key_ID\":\"$kid\"}]}" \
          "https://127.0.0.1:$(sae_port "$s")/api/v1/keys/sae_$m/dec_keys" 2>&1)
    slave_key=$(jq -r '.keys[0].key // empty' <<<"$dec" 2>/dev/null)
    if [ -z "$slave_key" ]; then
        fails+=("sae_$m→sae_$s dec_keys: $(head -c 100 <<<"$dec" | tr -d '\n')")
        bad=$((bad+1)); continue
    fi

    if [ "$master_key" = "$slave_key" ]; then
        ok=$((ok+1))
    else
        fails+=("sae_$m→sae_$s LOS BYTES NO COINCIDEN key_ID=$kid")
        bad=$((bad+1))
    fi
  done
done

n=${#NODES[@]}
echo "  pares ordenados: $(( n * (n-1) ))   idénticos: $ok   fallidos: $bad"
for f in "${fails[@]}"; do echo "    ✗ $f"; done
(( bad == 0 ))
