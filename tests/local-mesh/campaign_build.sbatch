#!/bin/bash
#SBATCH -J dkms-c9-build
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 32
#SBATCH --mem=24G
#SBATCH -t 00:50:00
#SBATCH -o campaign-2026-09/slurm/%x_%j.out
#SBATCH -e campaign-2026-09/slurm/%x_%j.out
#
# Compila una vez antes de las celdas (que dependen de este job con
# afterok): sin esto 60 celdas arrancarían a la vez compilando el mismo
# target/ y se pisarían por el lock de cargo durante 10 minutos cada una.
set -e
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
echo "########## build · $(hostname) · $(date '+%F %T') · GIT_SHA=$(cat GIT_SHA 2>/dev/null) ##########"
rustc --version; "$PROTOC" --version
time cargo build --release -j "$(nproc)"
ls -la target/release/sdn target/release/qkc target/release/orr target/release/dkms target/release/quditto target/release/sae_load
# Sanidad del generador de topologías y del renderer con el python del nodo.
python3 --version
for f in estrella anillo puente malla rgg aleatoria; do
    python3 tests/local-mesh/topologies.py "$f" 100 --meta | python3 -c 'import json,sys; m=json.load(sys.stdin); print(m["family"], m["n"], m["edges"], m["techo_fibra_keys_per_s"])'
done
echo "########## build OK $(date '+%F %T') ##########"
