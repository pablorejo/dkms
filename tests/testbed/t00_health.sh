#!/usr/bin/env bash
# T00 — salud del testbed y línea base.
#
# No toca nada. Vuelca el estado a tests/results/$CAMPAIGN/baseline/ y afirma
# los invariantes que tienen que cumplirse ANTES de cualquier otra prueba y
# otra vez DESPUÉS de todas (comparar los dos vuelcos es la mitad del valor).
#
#   ./t00_health.sh                 # línea base
#   ./t00_health.sh --tag post      # segundo vuelco, para diff contra el primero
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

TAG=baseline
[[ "${1:-}" == "--tag" ]] && TAG="${2:?falta el nombre del tag}"
DIR="$(mkoutdir "$TAG")"
info "vuelco en $DIR"

EXPECTED_NODES="${EXPECTED_NODES:-3}"
EXPECTED_EDGES="${EXPECTED_EDGES:-3}"

# ─── SDN ──────────────────────────────────────────────────────────────
for ep in topology qkcs orrs dkms saes links wcmp; do
    sdn_get "/$ep" > "$DIR/sdn_$ep.json" 2>/dev/null || echo '{}' > "$DIR/sdn_$ep.json"
done

topo=$(cat "$DIR/sdn_topology.json")
check "SDN: QKCs registrados"  "$(jq -r '.qkcs  // 0' <<<"$topo")" "$EXPECTED_NODES"
check "SDN: ORRs registrados"  "$(jq -r '.orrs  // 0' <<<"$topo")" "$EXPECTED_NODES"
check "SDN: DKMS registrados"  "$(jq -r '.dkms  // 0' <<<"$topo")" "$EXPECTED_NODES"
check "SDN: SAEs registrados"  "$(jq -r '.saes  // 0' <<<"$topo")" "$EXPECTED_NODES"
check "SDN: aristas del grafo" "$(jq -r '.edges // 0' <<<"$topo")" "$EXPECTED_EDGES"
info "topology.version = $(jq -r '.version // "?"' <<<"$topo")"

# ─── por nodo ─────────────────────────────────────────────────────────
for n in "${NODES[@]}"; do
    qkc="${NODE_QKC[$n]}"
    info "── $n (qkc $qkc / ${NODE_DKMS[$n]})"

    # Contenedores vivos. `docker ps` lista también los que están rebotando en
    # bucle, así que contar nombres no basta: hay que mirar el estado y el
    # contador de reinicios, o una imagen que no arranca pasa por sana.
    on "$n" "docker inspect -f '{{.Name}} {{.State.Status}} restarts={{.RestartCount}} {{.Config.Image}}' \
             \$(docker ps -aq --filter 'name=site-')" > "$DIR/$n.containers.txt"
    running=$(count_matches ' running restarts=' "$DIR/$n.containers.txt")
    check "$n: contenedores site-* en estado running" "$running" "3"
    flapping=$(awk '{ for (i=1;i<=NF;i++) if ($i ~ /^restarts=/) { split($i,a,"="); if (a[2]+0 > 2) c++ } } END { print c+0 }' \
               "$DIR/$n.containers.txt")
    check "$n: contenedores sin rebotar (RestartCount ≤ 2)" "$flapping" "0"

    # admin del QKC
    qkc_get "$n" /stats            > "$DIR/$n.qkc_stats.json"      2>/dev/null || true
    qkc_get "$n" /forwarding-table > "$DIR/$n.qkc_forwarding.json" 2>/dev/null || true

    # (los frames indescifrables se evalúan al final, sobre una ventana)

    # últimas líneas de estado de cada módulo
    dlogs "$n" qkc  400 > "$DIR/$n.qkc.log"
    dlogs "$n" orr  400 > "$DIR/$n.orr.log"
    dlogs "$n" dkms 400 > "$DIR/$n.dkms.log"

    # keystore del QKC: un enc=0 dec=0 taken=0 sostenido es la firma del
    # enlace PQC muerto (ver README, fase 4).
    levels=$(grep -a 'keystore.levels' "$DIR/$n.qkc.log" | tail -4 || true)
    if [[ -z "$levels" ]]; then
        fail "$n: sin líneas keystore.levels en los últimos 400 logs"
    else
        printf '%s\n' "$levels" >&2
        dead=$(printf '%s\n' "$levels" | { grep -ac 'enc=0 dec=0 taken=0' || true; })
        check "$n: ningún peer con keystore a cero" "$dead" "0"
    fi

    # el síntoma que acompaña al enlace muerto
    dlogs_since "$n" qkc 5m > "$DIR/$n.qkc.5m.log" || true
    stuck=$(count_matches 'timeout waiting pqc-secret' "$DIR/$n.qkc.5m.log")
    check "$n: sin 'timeout waiting pqc-secret' en 5 min" "$stuck" "0"

    # generator del DKMS: una línea por peer cada 5 s
    gstate=$(grep -a 'generator.state' "$DIR/$n.dkms.log" | tail -"$(( (EXPECTED_NODES-1) * 2 ))" || true)
    if [[ -z "$gstate" ]]; then
        fail "$n: sin líneas generator.state"
    else
        printf '%s\n' "$gstate" >&2
        corrupt=$(printf '%s\n' "$gstate" | max_field 'recv_corrupt=[0-9]*')
        check "$n: recv_corrupt" "${corrupt:-?}" "0"
        # ack_pending alto y creciendo = el peer no ACKea; en reposo debe ser ~0
        maxack=$(printf '%s\n' "$gstate" | max_field 'ack_pending=[0-9]*')
        info "$n: ack_pending máximo observado = ${maxack:-?}"
        # se registran, no se afirman: son las anomalías que T20/T12 acotan
        info "$n: dec máximo = $(printf '%s\n' "$gstate" | max_field ' dec=[0-9]*')"
        info "$n: expired máximo = $(printf '%s\n' "$gstate" | max_field 'expired=[0-9]*')"

        # Un DKMS que nunca ha recibido nada de nadie no tiene por dónde
        # devolver ACKs: se queda sin peer_ack_endpoint y su pareja acaba
        # expirando todo lo que emite. Es un fallo mudo — la topología de la
        # SDN sigue pintando perfecta — así que se afirma explícitamente.
        if grep -qa 'peer_ack_endpoint="<sin recibir>"' <<<"$gstate"; then
            fail "$n: algún peer con peer_ack_endpoint=<sin recibir> — nunca ha recibido un DKMS_BUFFER de él"
        else
            pass "$n: peer_ack_endpoint conocido para todos sus peers"
        fi
    fi

    # pánicos
    panics=$(count_matches 'panicked at' "$DIR/$n.qkc.log" "$DIR/$n.orr.log" "$DIR/$n.dkms.log")
    check "$n: sin panics en los logs recientes" "$panics" "0"
done

# ─── SDN: la rate que está repartiendo ────────────────────────────────
for n in "${NODES[@]}"; do
    sdn_get "/rate/${NODE_DKMS[$n]}" > "$DIR/rate_${NODE_DKMS[$n]}.json" 2>/dev/null || true
done
sdn_get /demand > "$DIR/sdn_demand.json" 2>/dev/null || true
dlogs "$SDN_HOST" sdn 400 > "$DIR/sdn.log" 2>/dev/null || \
    on "$SDN_HOST" "docker logs --tail 400 sdn-sdn-1 2>&1 | sed 's/\x1b\[[0-9;]*m//g'" > "$DIR/sdn.log"

# El Infeasible falso de microlp en fase 2 es conocido y tiene fallback η=0.
# No es un FAIL; se cuenta para poder decir si empeora al crecer la topología.
inf=$(count_matches 'nfeasible' "$DIR/sdn.log")
info "SDN: $inf líneas con 'infeasible' (bug microlp conocido, fallback η=0)"

# ─── frames que llegan y no se pueden descifrar ───────────────────────
#
# Tráfico se cuenta con `dec_lookups`; pérdida, con `wait_dec_timeouts` (la
# clave no apareció nunca). `dec_misses` no vale de métrica —solo dice que aún
# no estaba materializada y `wait_dec` la resuelve— y `wait_dec_called` menos
# todavía: en un enlace sano no se llama ni una vez, así que de denominador
# hacía que "perfecto" se leyera como "sin datos".
#
# Y hay que medirlo sobre una VENTANA, no sobre el acumulado. Los contadores
# viven desde que arrancó el contenedor, así que un enlace que se rompió hace
# diez minutos y ya se recuperó sigue mostrando sus pérdidas para siempre: con
# el acumulado, T00 daba tres FAIL sobre un despliegue que en ese momento no
# perdía un solo frame.
info "── ¿algún enlace tira lo que recibe? (ventana de ${DEC_WINDOW_SECS:-25} s)"
win_snapshot() {
    for n in "${NODES[@]}"; do
        printf '%s ' "$n"
        qkc_get "$n" /stats 2>/dev/null \
            | jq -rc '[.links // {} | to_entries[]
                       | "\(.key):\(.value.dec_lookups):\(.value.wait_dec_timeouts)"] | join(" ")' \
            2>/dev/null || echo ''
    done
}
win_snapshot > "$DIR/dec.win1"
sleep "${DEC_WINDOW_SECS:-25}"
win_snapshot > "$DIR/dec.win2"

rc_dec=0
python3 - "$DIR/dec.win1" "$DIR/dec.win2" >&2 <<'PY' || rc_dec=$?
import sys

def parse(path):
    out = {}
    for line in open(path):
        parts = line.split()
        for entry in parts[1:]:
            peer, lookups, timeouts = entry.split(":")
            out[(parts[0], peer)] = (int(lookups), int(timeouts))
    return out

a, b = parse(sys.argv[1]), parse(sys.argv[2])
bad = 0
for key in sorted(b):
    c0, t0 = a.get(key, (0, 0))
    dc, dt = b[key][0] - c0, b[key][1] - t0
    node, peer = key
    if dc <= 0:
        print(f"  {node} → {peer}: sin tráfico en la ventana")
        continue
    pct = 100 * dt / dc
    flag = "   ← TIRA LO QUE RECIBE" if pct > 5 else ""
    print(f"  {node} → {peer}: {dt}/{dc} frames perdidos ({pct:.1f} %){flag}")
    if pct > 5:
        bad += 1
sys.exit(1 if bad else 0)
PY
if (( rc_dec == 0 )); then
    pass "ningún enlace está tirando frames ahora mismo"
else
    fail "hay enlaces tirando lo que reciben: ventanas de épocas desincronizadas"
fi

# ─── reconciliación entre los dos extremos de cada par ────────────────
# Cada DKMS cuenta lo que emite hacia cada peer y lo que recibe de cada peer.
# Los dos números tienen que casar: lo que X emite hacia Y aparece en el recv
# de Y. Mirar un solo log no lo detecta —los contadores de X pueden verse
# sanos mientras Y no recibe nada—, y por eso este cruce va aparte.
info "── ¿cuadra lo que cada uno emite con lo que su pareja recibe?"
rc=0
python3 - "$DIR" "${NODES[@]}" >&2 <<'PY' || rc=$?
import pathlib, re, sys

d, nodes = pathlib.Path(sys.argv[1]), sys.argv[2:]

# nodo -> {peer_id: campos}, quedándose con la última línea de cada peer
state = {}
for n in nodes:
    f = d / f"{n}.dkms.log"
    if not f.exists():
        continue
    per = {}
    for line in f.read_text(errors="replace").splitlines():
        if "generator.state" not in line:
            continue
        kv = dict((k, v) for k, _q, v in re.findall(r'(\w+)=("?)([^\s"]*)\2', line))
        if "peer" in kv:
            per[kv["peer"]] = kv
    state[n] = per

# El id propio de cada nodo es el único que los demás nombran y él no.
ids = set().union(*(set(p) for p in state.values())) if state else set()
owner = {}
for n, per in state.items():
    mine = ids - set(per)
    if len(mine) == 1:
        owner[mine.pop()] = n

bad = 0
for n, per in sorted(state.items()):
    for peer, kv in sorted(per.items()):
        emitted = int(kv.get("emitted", 0))
        expired = int(kv.get("expired", 0))
        host = owner.get(peer)
        recv = None
        if host in state:
            mine = [k for k in state[host] if owner.get(k) == n]
            if mine:
                recv = int(state[host][mine[0]].get("recv", 0))
        line = f"  {n} → {peer}: emitted={emitted} expired={expired}"
        if recv is None:
            print(line + "  (sin log del otro extremo)")
            continue
        line += f"  |  el peer dice recv={recv}"
        if emitted > 100 and recv == 0:
            line += "   ← NADA LLEGA"
            bad += 1
        elif emitted and expired / emitted > 0.5:
            line += f"   ← {100 * expired / emitted:.0f}% expiradas"
            bad += 1
        print(line)

print(f"  ({bad} pares con problema)" if bad else "  (todos los pares cuadran)")
sys.exit(1 if bad else 0)
PY
if (( rc == 0 )); then
    pass "reconciliación emitted↔recv entre todos los pares"
else
    fail "hay pares donde lo emitido no llega, o expira más de la mitad (ver arriba)"
fi
info "SDN: $inf líneas con 'infeasible' en los últimos 400 logs (bug microlp conocido, fallback η=0)"

summary
