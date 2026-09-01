#!/bin/bash
#SBATCH -J dkms-stress
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 32
#SBATCH --mem=16G
#SBATCH -t 01:30:00
# singleton: dos de estas campañas NUNCA pueden compartir nodo — la malla usa
# puertos fijos de 127.0.0.1, y dos jobs concurrentes se conectan a los módulos
# del otro (medido 2026-08-25, jobs 9275360/9275361: 520k SSLCertVerificationError
# porque los loaders de uno hablaban con los DKMS del otro, y un OOM por las dos
# mallas en un nodo). Con singleton, jobs con este mismo nombre corren de uno en
# uno; para paralelizar de verdad habría que parametrizar los puertos.
#SBATCH -d singleton
#SBATCH -o %x_%j.out
#SBATCH -e %x_%j.out
#
# Campaña de medida en un nodo de cómputo del FT3 (CESGA).
#
#   sbatch --export=ARM=A tests/local-mesh/cesga.sbatch
#   sbatch --export=ARM=B,TOKENS=3200 tests/local-mesh/cesga.sbatch
#   sbatch --export=ARM=A,TOPO=star tests/local-mesh/cesga.sbatch
#   sbatch --export=ARM=A,LINK_TYPE=qkd tests/local-mesh/cesga.sbatch
#
# 32 cores y 16 GB, y NO `--exclusive`: pedir el nodo entero en el FT3 lo manda
# a una cola de días —medido, un inicio estimado a >20 h— y con 31 módulos más
# unos 60 procesos de carga sobran de largo. El riesgo de compartir nodo es que
# se mida al vecino, así que el muestreo de `stress.sh` recoge `%CPU` cada 5 s:
# si la CPU no está saturada por nosotros, la contención se ve.
#
# 16 GB le bastan de sobra a la rama A: las tres campañas medidas el 2026-08-21
# (anillo, estrella y aleatoria, 10 nodos) picaron sobre 6 GB. A la rama B NO:
# con el techo ×10 el generador produce más rápido de lo que la cadena ORR→QKC
# transporta, las colas crecen sin freno y el trabajo murió por OOM a los 16,6
# GB. Subir la memoria sólo retrasa el final — lo que falta es contrapresión
# entre el generador y el transporte. Si repites la rama B, pide 64 GB Y
# quédate en 1 hilo por par, que es donde sí se sostiene (21 714 claves/s
# agregadas, cero rechazos).
#
# El tope de memoria lo pone SLURM, así que `mesh.sh` detecta que no puede
# crear su scope de systemd —dentro de un trabajo no hay bus de sesión de
# usuario— y se apoya en el cgroup del trabajo.
#
# Dos cosas del FT3 que no son obvias y cuestan una tarde si no se saben:
#
#   * el gcc del sistema es 10.1.0, que arrastra el bug de memcmp
#     https://gcc.gnu.org/bugzilla/show_bug.cgi?id=95189 — `aws-lc-sys`, la
#     libcrypto de rustls, se niega a compilar con él, y con razón. Hace falta
#     `gcc/12.3.0`.
#   * el protoc del sistema es 3.14 y los campos `optional` de proto3 no son
#     estables hasta la 3.15, así que `common/build.rs` falla. Se usa uno
#     moderno del home.

set -e
ARM="${ARM:-A}"
TOKENS="${TOKENS:-3200}"
NODES="${NODES:-10}"
TOPO="${TOPO:-ring}"
LINK_TYPE="${LINK_TYPE:-pqc}"
THREADS="${THREADS:-1 4 16}"
DURATION="${DURATION:-300}"
# Reposo medido antes del barrido (0 = ninguno). Es el tercer régimen de
# carga: la malla viva sin NADA de tráfico SAE.
IDLE="${IDLE:-0}"

# cmake: lo necesita highs-sys (backend `highs` de good-lp) para compilar HiGHS.
# El módulo de rust puede no cargar si el montaje del árbol de módulos está
# convaleciente (medido 2026-08-26: 40 jobs muertos en 3 s tras la avería del
# clúster — el load moría entre binutils y gcc SOLO en los nodos de cómputo).
# Reintentar con paciencia convierte esa muerte instantánea en una espera.
for intento in $(seq 1 15); do
    module load cmake gcc/12.3.0 rust/1.88.0 2>&1 || true
    command -v rustc >/dev/null 2>&1 && rustc --version >/dev/null 2>&1 && break
    echo "módulos incompletos (intento $intento/15); reintento en 60 s"
    sleep 60
done
command -v rustc >/dev/null || { echo "FATAL: rust no cargó tras 15 intentos"; exit 1; }
export CC=gcc CXX=g++
export PROTOC="${PROTOC:-$HOME/opt/protoc/bin/protoc}"

cd "$HOME/dkms_rust" || exit 1

# Los artefactos al scratch local del nodo: el home va por NFS y aquí hay 31
# procesos escribiendo logs más los CSV de la carga, que a 16 hilos por par son
# millones de filas.
export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-mesh-$SLURM_JOB_ID"
export DKMS_STRESS_OUT="${TMPDIR:-/tmp}/dkms-stress-$SLURM_JOB_ID"
mkdir -p "$DKMS_STRESS_OUT"

export DKMS_MESH_TOPO="$TOPO" DKMS_MESH_LINK_TYPE="$LINK_TYPE"
echo "########## $(hostname) · $(nproc) cpus · rama $ARM · topología $TOPO · $(date '+%F %T') ##########"
gcc --version | head -1
rustc --version
"$PROTOC" --version

# Compilar SIEMPRE. El guardia anterior miraba si existía `target/release/sdn`
# y se saltaba el build si estaba — así que borrar SÓLO el binario del módulo
# que habías cambiado no recompilaba nada, y la campaña corría con un binario
# ausente: medido el 2026-08-24, 51 minutos de trabajo con cero DKMS
# escuchando y 11 millones de ConnectionRefused. Un build sin cambios le
# cuesta a cargo un segundo; el guardia no ahorraba nada y costaba campañas.
echo "=== compilando (incremental) ==="
time cargo build --release -j "$(nproc)"

ARGS=(--arm "$ARM" --nodes "$NODES" --threads "$THREADS" --duration "$DURATION")
[ "$ARM" = B ] && ARGS+=(--tokens "$TOKENS")
[ "$IDLE" -gt 0 ] 2>/dev/null && ARGS+=(--idle "$IDLE")

# Rescate incremental del informe. Los artefactos viven en el scratch del nodo
# y solo se copian al home AL FINAL: un trabajo que muere en el último punto
# del barrido (OOM, límite de tiempo) se llevaba por delante también las fases
# que SÍ habían terminado. Con esto, lo medido hasta el momento sobrevive.
DEST_EARLY="$HOME/dkms_rust/results-$ARM-$TOPO-$LINK_TYPE-$SLURM_JOB_ID"
mkdir -p "$DEST_EARLY"
( while sleep 60; do
    for d in "$DKMS_STRESS_OUT"/stress-"$ARM"-*; do
        [ -f "$d/report.txt" ] && cp "$d/report.txt" "$DEST_EARLY/report.partial.txt" 2>/dev/null
    done
  done ) &
RESCUER=$!
trap 'kill $RESCUER 2>/dev/null' EXIT
./tests/local-mesh/stress.sh "${ARGS[@]}"

# El informe y los CSV comprimidos al home; el scratch del nodo se va con el
# trabajo.
DEST="$HOME/dkms_rust/results-$ARM-$TOPO-$LINK_TYPE-$SLURM_JOB_ID"
mkdir -p "$DEST"
for d in "$DKMS_STRESS_OUT"/stress-"$ARM"-*; do
    [ -d "$d" ] || continue
    cp "$d/report.txt" "$DEST/" 2>/dev/null || true
    tar czf "$DEST/$(basename "$d").tar.gz" -C "$(dirname "$d")" "$(basename "$d")" 2>/dev/null || true
done
echo "resultados en $DEST"
echo "########## fin $(date '+%F %T') ##########"
