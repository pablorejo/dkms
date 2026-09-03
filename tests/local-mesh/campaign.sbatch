#!/bin/bash
#SBATCH -J dkms-c9
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 64
#SBATCH -o campaign-2026-09/slurm/%x_%j.out
#SBATCH -e campaign-2026-09/slurm/%x_%j.out
#
# Una celda de la campaña 2026-09 en un nodo de cómputo del FT3:
#
#   sbatch --mem=48G -t 01:15:00 --export=ALL,FAM=anillo,N=10 tests/local-mesh/campaign.sbatch
#
# Memoria y walltime los pone campaign_submit.sh según N. `-c 64` pide el
# nodo entero (los del short son de 64 cores): así dos celdas nunca comparten
# nodo, que era lo que cruzaba las mallas por los puertos fijos de 127.0.0.1
# (jobs 9275360/9275361, 2026-08-25) — sin necesidad de `-d singleton`, y las
# celdas corren en paralelo en nodos distintos.
set -e
FAM="${FAM:?}"
N="${N:?}"

# cmake: highs-sys. gcc 12.3: aws-lc-sys. protoc moderno: proto3 optional.
# El load de módulos muere a veces en los nodos de cómputo (2026-08-26): la
# comprobación va DENTRO de un if, o `set -e` mata el job antes del reintento.
for intento in $(seq 1 15); do
    module load cmake gcc/12.3.0 rust/1.88.0 2>&1 || true
    if command -v rustc >/dev/null 2>&1 && rustc --version >/dev/null 2>&1; then
        break
    fi
    echo "módulos incompletos (intento $intento/15); reintento en 60 s"
    sleep 60
done
command -v rustc >/dev/null || { echo "FATAL: rust no cargó tras 15 intentos"; exit 1; }
export CC=gcc CXX=g++
export PROTOC="${PROTOC:-$HOME/opt/protoc/bin/protoc}"

cd "$HOME/dkms_rust" || exit 1

# Malla al scratch local del nodo (logs de 300+ procesos); resultados
# reducidos al home, que sí sobrevive al job.
export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-c9-$SLURM_JOB_ID"
export DKMS_CAMPAIGN_OUT="${DKMS_CAMPAIGN_OUT:-$HOME/dkms_rust/campaign-2026-09/cells}"
mkdir -p "$DKMS_CAMPAIGN_OUT"

echo "########## $(hostname) · $(nproc) cpus · $FAM N=$N · $(date '+%F %T') ##########"
rustc --version; "$PROTOC" --version
# Compilar SIEMPRE (incremental): tras el job de build es un no-op de 1 s, y
# nunca corre con un binario ausente o viejo (medido 2026-08-24: 51 min de
# ConnectionRefused por un guard "si existe el binario, no compiles").
echo "=== compilando (incremental) ==="
time cargo build --release -j "$(nproc)"

bash tests/local-mesh/campaign_cell.sh "$FAM" "$N"
rc=$?
rm -rf "$DKMS_MESH_DIR"
echo "########## fin $FAM N=$N rc=$rc $(date '+%F %T') ##########"
exit $rc
