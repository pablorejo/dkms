#!/bin/bash
#SBATCH -J dkms-stress
#SBATCH -p short
#SBATCH -N 1
#SBATCH --exclusive
#SBATCH --mem=32G
#SBATCH -t 01:30:00
#SBATCH -o %x_%j.out
#SBATCH -e %x_%j.out
#
# Campaña de medida en un nodo de cómputo del FT3 (CESGA).
#
#   sbatch --export=ARM=A tests/local-mesh/cesga.sbatch
#   sbatch --export=ARM=B,TOKENS=3200 tests/local-mesh/cesga.sbatch
#
# `--exclusive`: con 90 pares ordenados bajo carga, si el nodo va compartido lo
# que se mide es el vecino. El tope de memoria lo pone SLURM, así que `mesh.sh`
# detecta que no puede crear su scope de systemd —dentro de un trabajo no hay
# bus de sesión de usuario— y se apoya en el cgroup del trabajo.
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
THREADS="${THREADS:-1 4 16}"
DURATION="${DURATION:-300}"

module load gcc/12.3.0 rust/1.88.0
export CC=gcc CXX=g++
export PROTOC="${PROTOC:-$HOME/opt/protoc/bin/protoc}"

cd "$HOME/dkms_rust" || exit 1

# Los artefactos al scratch local del nodo: el home va por NFS y aquí hay 31
# procesos escribiendo logs más los CSV de la carga, que a 16 hilos por par son
# millones de filas.
export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-mesh-$SLURM_JOB_ID"
export DKMS_STRESS_OUT="${TMPDIR:-/tmp}/dkms-stress-$SLURM_JOB_ID"
mkdir -p "$DKMS_STRESS_OUT"

echo "########## $(hostname) · $(nproc) cpus · rama $ARM · $(date '+%F %T') ##########"
gcc --version | head -1
rustc --version
"$PROTOC" --version

if [ ! -x target/release/sdn ]; then
    echo "=== compilando ==="
    time cargo build --release -j "$(nproc)"
fi

ARGS=(--arm "$ARM" --nodes "$NODES" --threads "$THREADS" --duration "$DURATION")
[ "$ARM" = B ] && ARGS+=(--tokens "$TOKENS")
./tests/local-mesh/stress.sh "${ARGS[@]}"

# El informe y los CSV comprimidos al home; el scratch del nodo se va con el
# trabajo.
DEST="$HOME/dkms_rust/results-$ARM-$SLURM_JOB_ID"
mkdir -p "$DEST"
for d in "$DKMS_STRESS_OUT"/stress-"$ARM"-*; do
    [ -d "$d" ] || continue
    cp "$d/report.txt" "$DEST/" 2>/dev/null || true
    tar czf "$DEST/$(basename "$d").tar.gz" -C "$(dirname "$d")" "$(basename "$d")" 2>/dev/null || true
done
echo "resultados en $DEST"
echo "########## fin $(date '+%F %T') ##########"
