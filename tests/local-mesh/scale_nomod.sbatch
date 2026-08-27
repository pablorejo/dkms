#!/bin/bash
#SBATCH -J dkms-scale
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 32
#SBATCH -o %x_%j.out
#SBATCH -e %x_%j.out
#SBATCH -d singleton
#
# Variante SIN MÓDULOS de scale.sbatch, nacida el 2026-08-26: tras la avería
# del clúster, el montaje del árbol de software (netapp1/Optcesga) quedó
# convaleciente en un subconjunto CAMBIANTE de nodos de cómputo — 80 jobs
# murieron en 3-12 s en tres oleadas, cada una en nodos distintos, al no poder
# cargar el módulo de rust. Este job no toca netapp1: usa los binarios YA
# compilados del home (netapp2, sano) y el python del sistema (/usr/bin,
# disco local, 3.6.8 — los scripts son compatibles). Sin rust, sin gcc, sin
# cmake, sin protoc: nada que compilar, nada que enfermar.
#
#   sbatch --mem=32G -t 01:15:00 --export=TOPO=cn,N=40,REG=poca \
#       tests/local-mesh/scale_nomod.sbatch

set -e
TOPO="${TOPO:?}"
N="${N:?}"
REG="${REG:?}"

# El python del sistema por delante del gentoo de netapp1.
export PATH=/usr/bin:$PATH
# La SDN (HiGHS, C++ de gcc-12) necesita GLIBCXX_3.4.30: va empaquetado en
# ~/dkms_rust/lib (copiado una vez del módulo de gcc en el login, vive en el
# home sano de netapp2) — sin esto el binario no arranca fuera del entorno de
# módulos, medido 2026-08-26 en 7 jobs ("mesh: la SDN no arrancó").
export LD_LIBRARY_PATH="$HOME/dkms_rust/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

cd "$HOME/dkms_rust" || exit 1
for b in sdn qkc orr dkms quditto; do
    [ -x "target/release/$b" ] || { echo "FATAL: falta target/release/$b (compilar aparte)"; exit 1; }
done
python3 -c 'import json' || { echo "FATAL: python3 inutilizable"; exit 1; }

export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-scale-$SLURM_JOB_ID"
export DKMS_SCALE_OUT="$HOME/dkms_rust/scale-results"
mkdir -p "$DKMS_SCALE_OUT"

echo "########## $(hostname) · $TOPO n=$N $REG · sin módulos · $(date '+%F %T') ##########"
bash tests/local-mesh/scale_one.sh "$TOPO" "$N" "$REG"
echo "########## fin $(date '+%F %T') ##########"
