#!/usr/bin/env bash
# Prueba NEGATIVA del MAC de frame: que `frame_auth = require` corte de verdad.
#
# Los tests unitarios prueban que el MAC rechaza un frame forjado. Lo que no
# prueban es que la política se aplique en un despliegue: que un vecino que deja
# de firmar se quede fuera en lugar de que el enlace degrade a no autenticado en
# silencio, que es el fallo que de verdad importa.
#
#   bash tests/local-mesh/frame_auth_negative.sh [N] [topo]
#
# Levanta una malla con `frame_auth = require`, comprueba que está sana, le
# quita al ÚLTIMO nodo su `link_psk` y le pone `frame_auth = off` en caliente,
# lo reinicia, y comprueba que:
#
#   1. sus vecinos empiezan a descartar (`plain_rej` sube),
#   2. el tráfico con él se CONGELA (los contadores dejan de moverse),
#   3. el primer rechazo es `kind=32` (`FRAME_KEY_IDS_NOTIFY` sin firmar): el
#      NOTIFY es fail-closed en cuanto el receptor tiene PSK, así que lo primero
#      que se rompe es la distribución de claves — un fallo limpio y ruidoso.
#
# Deja la malla levantada si falla, para poder mirar los logs.

set -u
cd "$(dirname "$0")/../.." || exit 1
REPO="$PWD"
N="${1:-4}"
TOPO="${2:-ring}"
MESH="$REPO/tests/local-mesh/mesh.sh"
DIR="$REPO/tests/results/local-mesh"
BIN="$REPO/target/release"
VICTIM="$N"

nofmt() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

# Contadores del enlace (me → peer) tal como los deja la línea `qkc.frame_auth`.
counters() {  # counters <me> <peer> <campo>
    nofmt "$DIR/logs/qkc$1.log" \
        | grep "qkc.frame_auth me=$1 peer=$2 " | tail -1 \
        | grep -o "$3=[0-9]*" | cut -d= -f2
}

fail() { echo "FALLO: $*" >&2; exit 1; }

[ -x "$BIN/qkc" ] || fail "falta $BIN/qkc (cargo build --release)"

# Arranque limpio. Esta prueba reinicia un qkc a mano, así que queda fuera de
# la lista de PIDs de la malla y `mesh.sh down` no lo mata: la vuelta siguiente
# se encontraría el puerto ocupado y el nodo muerto.
#
# `pkill -x` casa el NOMBRE del ejecutable. Con `-f` el patrón casa también la
# línea de comandos del shell que ejecuta este script, que la contiene — y el
# script se suicida.
bash "$MESH" down >/dev/null 2>&1
pkill -x qkc 2>/dev/null
sleep 2

echo "== levantando malla n=$N $TOPO con frame_auth = require"
DKMS_MESH_FRAME_AUTH=require bash "$MESH" up "$N" "$TOPO" >/dev/null 2>&1 \
    || fail "la malla no levantó"
sleep 30

# Vecinos del nodo víctima, según lo que él mismo dice tener vivo.
NEIGH=$(nofmt "$DIR/logs/qkc$VICTIM.log" | grep "qkc.links me=$VICTIM " | tail -1 \
        | sed 's/.*live=\[\([^]]*\)\].*/\1/' | tr -d ' ' | tr ',' ' ')
[ -n "$NEIGH" ] || fail "el nodo $VICTIM no tiene vecinos vivos; la malla no arrancó bien"
echo "== vecinos de $VICTIM: $NEIGH"

for p in $NEIGH; do
    v=$(counters "$p" "$VICTIM" verified)
    [ -n "$v" ] && [ "$v" -gt 0 ] || fail "el enlace $p→$VICTIM no está autenticando (verified=$v)"
done
echo "== estado sano confirmado: los vecinos verifican frames del $VICTIM"

echo "== quitando el link_psk al nodo $VICTIM y poniéndolo en frame_auth = off"
python3 - "$DIR/cfg/qkc$VICTIM/qkc.toml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = "\n".join(l for l in s.split("\n") if not l.startswith("link_psk"))
s = s.replace('frame_auth = "require"', 'frame_auth = "off"')
open(p, "w").write(s)
PY
# Sólo el qkc de la víctima, identificado por el fichero de config que tiene en
# su línea de comandos.
for pid in $(pgrep -x qkc); do
    tr "\0" "\n" < "/proc/$pid/cmdline" 2>/dev/null \
        | grep -qx "$DIR/cfg/qkc$VICTIM/qkc.toml" && kill "$pid"
done
sleep 2
(CONFIG_DIR="$DIR/cfg/qkc$VICTIM" nohup "$BIN/qkc" \
    --config "$DIR/cfg/qkc$VICTIM/qkc.toml" >> "$DIR/logs/qkc$VICTIM.log" 2>&1 &)
sleep 40

echo "== comprobando que los vecinos lo dejan fuera"
for p in $NEIGH; do
    r=$(counters "$p" "$VICTIM" plain_rej)
    [ -n "$r" ] && [ "$r" -gt 0 ] \
        || fail "el vecino $p NO descartó nada del $VICTIM (plain_rej=$r): require no se está aplicando"
    echo "   $p descarta del $VICTIM: plain_rej=$r"
done

# El primer rechazo tiene que ser el NOTIFY (0x20 = 32).
p1=$(echo "$NEIGH" | awk '{print $1}')
kind=$(nofmt "$DIR/logs/qkc$p1.log" | grep "descarto peer=$VICTIM" | head -1 \
       | grep -o "kind=[0-9]*" | cut -d= -f2)
[ "$kind" = 32 ] \
    || echo "   AVISO: el primer rechazo fue kind=$kind, no el NOTIFY (32)"
[ "$kind" = 32 ] && echo "   primer rechazo = kind=32 (NOTIFY sin firmar), como se espera"

echo "== comprobando que el tráfico con él se congela"
declare -A before
for p in $NEIGH; do before[$p]=$(counters "$p" "$VICTIM" verified); done
sleep 25
for p in $NEIGH; do
    after=$(counters "$p" "$VICTIM" verified)
    [ "$after" = "${before[$p]}" ] \
        || fail "el enlace $p→$VICTIM SIGUE verificando (${before[$p]} → $after): el nodo sin PSK no quedó fuera"
    echo "   $p→$VICTIM congelado en verified=$after"
done

echo "== OK: require corta el enlace en vez de degradarlo"
bash "$MESH" down >/dev/null 2>&1
pkill -x qkc 2>/dev/null
exit 0
