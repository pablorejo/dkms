#!/usr/bin/env bash
# Prueba de INTEGRIDAD end-to-end: un atacante en el cable modifica frames y el
# MAC lo detecta antes de que la corrupción llegue al material de clave.
#
#   bash tests/local-mesh/frame_auth_tamper.sh [N] [topo]
#
# Los tests unitarios comprueban que `frame_mac::verify` rechaza un tag que no
# cuadra. Lo que no comprueban es lo que de verdad promete la propiedad: que en
# un sistema en marcha, alguien que sólo puede tocar bytes del cable no consiga
# colar una modificación. Sin MAC eso es indetectable —el OTP es maleable, así
# que un bit volteado en el ciphertext sale como un bit volteado en el
# plaintext— y la única defensa que quedaba era el `key_digest` del DKMS, que
# sólo cubre el camino del DKMS_BUFFER.
#
# Monta la malla con `frame_auth = require`, mete un proxy que voltea un bit
# cada N frames entre dos vecinos, y comprueba que:
#
#   1. `bad_mac` sube en el peer que recibe los frames tocados,
#   2. `recv_corrupt` del DKMS se queda en 0 — la corrupción NO llega al
#      material, que es lo que importa,
#   3. el resto de la malla sigue funcionando (los demás enlaces, intactos).

set -u
cd "$(dirname "$0")/../.." || exit 1
REPO="$PWD"
N="${1:-4}"
TOPO="${2:-ring}"
MESH="$REPO/tests/local-mesh/mesh.sh"
DIR="$REPO/tests/results/local-mesh"
BIN="$REPO/target/release"
PROXY_PORT=29100
EVERY=20

nofmt() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
fail() { echo "FALLO: $*" >&2; exit 1; }
counters() {  # counters <me> <peer> <campo>
    nofmt "$DIR/logs/qkc$1.log" | grep "qkc.frame_auth me=$1 peer=$2 " | tail -1 \
        | grep -o "$3=[0-9]*" | cut -d= -f2
}

[ -x "$BIN/qkc" ] || fail "falta $BIN/qkc (cargo build --release)"

bash "$MESH" down >/dev/null 2>&1
pkill -x qkc 2>/dev/null
# Nada de `pkill -f frame_tamper.py`: el patrón casa también la línea de
# comandos del shell que lanza este script, y se lo lleva por delante. Se
# busca por el cmdline real en /proc, como con el qkc.
for pid in $(pgrep -x python3); do
    tr "\0" " " < "/proc/$pid/cmdline" 2>/dev/null \
        | grep -q "frame_tamper.py" && kill "$pid"
done
for p in orr dkms sdn; do pkill -x "$p" 2>/dev/null; done
# Esperar a que NO quede nada: si algún proceso sobrevive tiene el puerto
# cogido, `mesh.sh up` arranca a medias y —como no falla duro— devuelve 0 con
# la malla rota. Pasó: logs vacíos y cuatro qkc zombis de la vuelta anterior.
for _ in $(seq 1 20); do
    # `pgrep -c` ya imprime 0 cuando no encuentra nada, pero devuelve 1: un
    # `|| echo 0` añade un SEGUNDO cero y la aritmética revienta con "0\n0".
    vivos=0
    for p in qkc orr dkms sdn; do
        n=$(pgrep -c -x "$p" 2>/dev/null | head -1)
        vivos=$(( vivos + ${n:-0} ))
    done
    [ "$vivos" = 0 ] && break
    sleep 1
done
[ "$vivos" = 0 ] || fail "quedan $vivos procesos de una malla anterior; no arranco encima"

# Enlaces QKD: los declaran los dos extremos con dirección explícita, así que
# se puede redirigir uno por el proxy sin que la SDN lo deshaga.
echo "== levantando malla n=$N $TOPO (qkd) con frame_auth = require"
DKMS_MESH_FRAME_AUTH=require DKMS_MESH_LINK_TYPE=qkd \
    bash "$MESH" up "$N" "$TOPO" >/dev/null 2>&1 || fail "la malla no levantó"
[ -s "$DIR/logs/qkc$N.log" ] || fail "la malla dice que levantó pero no hay logs: arrancó a medias"
sleep 30

VICTIM=2          # el que recibirá los frames tocados
SENDER=1          # el que los emite
REAL_PORT=$(grep -A3 "neighbor_id = $VICTIM" "$DIR/cfg/qkc$SENDER/qkc.toml" \
            | grep neighbor_peer_addr | head -1 | sed 's/.*:\([0-9]*\)".*/\1/')
[ -n "$REAL_PORT" ] || fail "no encuentro el puerto de $SENDER→$VICTIM"
echo "== $SENDER habla con $VICTIM en 127.0.0.1:$REAL_PORT; lo desvío por el proxy"

v=$(counters "$VICTIM" "$SENDER" verified)
[ -n "$v" ] && [ "$v" -gt 0 ] || fail "el enlace $SENDER→$VICTIM no autentica (verified=$v)"

python3 "$REPO/tests/local-mesh/frame_tamper.py" --listen "$PROXY_PORT" \
    --to "127.0.0.1:$REAL_PORT" --every "$EVERY" > "$DIR/logs/tamper.log" 2>&1 &
PROXY_PID=$!
sleep 2
kill -0 "$PROXY_PID" 2>/dev/null || fail "el proxy no arrancó (ver $DIR/logs/tamper.log)"

# Redirige al emisor por el proxy y reinícialo.
python3 - "$DIR/cfg/qkc$SENDER/qkc.toml" "$REAL_PORT" "$PROXY_PORT" <<'PY'
import sys
p, real, proxy = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(p).read()
s = s.replace('127.0.0.1:%s' % real, '127.0.0.1:%s' % proxy)
open(p, 'w').write(s)
PY
for pid in $(pgrep -x qkc); do
    tr "\0" "\n" < "/proc/$pid/cmdline" 2>/dev/null \
        | grep -qx "$DIR/cfg/qkc$SENDER/qkc.toml" && kill "$pid"
done
sleep 2
(CONFIG_DIR="$DIR/cfg/qkc$SENDER" nohup "$BIN/qkc" \
    --config "$DIR/cfg/qkc$SENDER/qkc.toml" >> "$DIR/logs/qkc$SENDER.log" 2>&1 &)
sleep 45

echo "== proxy: $(tail -1 "$DIR/logs/tamper.log")"
bad=$(counters "$VICTIM" "$SENDER" bad_mac)
[ -n "$bad" ] && [ "$bad" -gt 0 ] \
    || fail "el peer $VICTIM NO detectó ninguna modificación (bad_mac=$bad). \
Con el OTP a pelo esto habría pasado desapercibido: es exactamente el agujero."
echo "   $VICTIM detecta y descarta lo modificado: bad_mac=$bad"

# Lo que de verdad importa: la corrupción no llega al material de clave.
corrupt=$(nofmt "$DIR/logs/dkms$VICTIM.log" | grep -o "recv_corrupt=[0-9]*" \
          | tail -1 | cut -d= -f2)
[ "${corrupt:-0}" = 0 ] \
    || fail "recv_corrupt=$corrupt en el DKMS $VICTIM: la corrupción LLEGÓ al material"
echo "   y no llega al DKMS: recv_corrupt=${corrupt:-0}"

echo "== OK: el atacante del cable no cuela una sola modificación"
kill "$PROXY_PID" 2>/dev/null
bash "$MESH" down >/dev/null 2>&1
pkill -x qkc 2>/dev/null
exit 0
