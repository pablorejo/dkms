#!/bin/bash
#SBATCH -J dkms-scale
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 32
#SBATCH -o %x_%j.out
#SBATCH -e %x_%j.out
# singleton: mismas razones que en cesga.sbatch — dos mallas en un nodo se
# cruzan por los puertos fijos de 127.0.0.1. Un job de escala cada vez.
#SBATCH -d singleton
#
# Un job por configuración de la campaña de escala:
#   sbatch --mem=16G -t 00:50:00 --export=TOPO=ringchords,N=20,REG=poca \
#       tests/local-mesh/scale.sbatch
# La memoria y el walltime los pone el que somete, escalados con N.

set -e
TOPO="${TOPO:?}"
N="${N:?}"
REG="${REG:?}"

# cmake: highs-sys. gcc 12.3: aws-lc-sys. protoc moderno: proto3 optional.
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

export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-scale-$SLURM_JOB_ID"
export DKMS_SCALE_OUT="$HOME/dkms_rust/scale-results"
mkdir -p "$DKMS_SCALE_OUT"

echo "########## $(hostname) · $TOPO n=$N $REG · $(date '+%F %T') ##########"
echo "=== compilando (incremental) ==="
time cargo build --release -j "$(nproc)"

bash tests/local-mesh/scale_one.sh "$TOPO" "$N" "$REG"
echo "########## fin $(date '+%F %T') ##########"
