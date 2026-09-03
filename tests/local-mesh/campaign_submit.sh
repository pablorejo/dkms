#!/usr/bin/env bash
# Somete la campaña 2026-09 completa en CESGA: 6 familias × N=10..100 (60
# celdas, cada una con sus 3 cargas), tras un job de build del que dependen.
# Idempotente: se salta las celdas con DONE. Se corre en el login node,
# dentro de ~/dkms_rust.
#
#   bash tests/local-mesh/campaign_submit.sh [familias...]   (default: las 6)
#   FAMS="anillo" NS="10 20" bash tests/local-mesh/campaign_submit.sh
set -euo pipefail
cd "$HOME/dkms_rust"
FAMS="${FAMS:-${*:-estrella anillo puente malla rgg aleatoria}}"
NS="${NS:-10 20 30 40 50 60 70 80 90 100}"
CELLS="${DKMS_CAMPAIGN_OUT:-$HOME/dkms_rust/campaign-2026-09/cells}"
mkdir -p campaign-2026-09/slurm "$CELLS"

# Recursos por N. MaxRSS medido a N=30 (stress con 13 920 hilos de cliente):
# 40-54 GB; aquí el cliente es un flujo por par, más ligero. Los nodos del
# short tienen 247 GB: 200G cabe en cualquiera de 64 cores.
res_for() {
    local n=$1
    if   (( n <= 30 )); then echo "--mem=48G  -t 01:15:00"
    elif (( n <= 60 )); then echo "--mem=96G  -t 01:45:00"
    else                     echo "--mem=200G -t 02:30:00"
    fi
}

BUILD=${BUILD_JOB:-}
if [ -z "$BUILD" ]; then
    BUILD=$(sbatch --parsable tests/local-mesh/campaign_build.sbatch)
    echo "build job: $BUILD"
fi
n_sub=0; n_skip=0
for n in $NS; do
    for fam in $FAMS; do
        if [ -f "$CELLS/$fam-n$n/DONE" ]; then n_skip=$(( n_skip + 1 )); continue; fi
        # shellcheck disable=SC2046
        jid=$(sbatch --parsable -J "dkms-c9-$fam-$n" $(res_for "$n") \
              --dependency=afterok:"$BUILD" --export=ALL,FAM="$fam",N="$n" \
              tests/local-mesh/campaign.sbatch)
        echo "$fam N=$n -> job $jid"
        echo "$jid $fam $n $(date +%s)" >> campaign-2026-09/submitted.txt
        n_sub=$(( n_sub + 1 ))
    done
done
echo "sometidas $n_sub celdas ($n_skip ya hechas); build $BUILD"
