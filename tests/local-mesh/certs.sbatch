#!/bin/bash
#SBATCH -J dkms-certs
#SBATCH -p short
#SBATCH -N 1
#SBATCH -c 32
#SBATCH -o %x_%j.out
#SBATCH -e %x_%j.out
# singleton: mismas razones que en cesga.sbatch/scale.sbatch — dos mallas en un
# nodo se cruzan por los puertos fijos de 127.0.0.1 (medido 2026-08-25: 520 k
# SSLCertVerificationError). Un job de esta campaña cada vez.
#SBATCH -d singleton
#
# Campaña de CERTIFICADOS: mide si el mTLS funciona y qué cuesta, comparando
# certificados clásicos (RSA) con post-cuánticos (ML-DSA-65, FIPS 204) sobre la
# MISMA topología, el mismo N y los mismos tres regímenes de carga.
#
#   sbatch --mem=32G -t 01:15:00 \
#       --export=TOPO=ringchords,N=30,REG=media,KEY_ALG=rsa \
#       tests/local-mesh/certs.sbatch
#   sbatch --mem=32G -t 01:15:00 \
#       --export=TOPO=ringchords,N=30,REG=media,KEY_ALG=ml-dsa-65 \
#       tests/local-mesh/certs.sbatch
#
# CERTIFICADOS ML-DSA: no se pueden generar aquí. El OpenSSL de CESGA es
# 1.1.1g (2020) y no conoce ML-DSA, así que `gen-certs.sh` fallaría. Se
# generan en una máquina con OpenSSL >= 3.5 y se traen ya hechos; este job los
# siembra con DKMS_MESH_CERTS_SRC. Por eso CERTS_DIR es obligatorio cuando
# KEY_ALG no es rsa.
#
# CLIENTE DE CARGA: por la misma razón, `sae_load.py` (módulo `ssl` = OpenSSL
# del sistema) no puede presentar un cert de cliente ML-DSA. `scale_one.sh`
# usa el binario Rust `target/release/sae_load` si existe, que habla mTLS con
# ambos. Este job verifica que el binario está antes de arrancar: sin él, el
# arm ML-DSA moriría a los 20 s en `check_load` y el job se tiraría a la basura.

set -e
TOPO="${TOPO:?falta TOPO (ringchords|cn|bridge2)}"
N="${N:?falta N}"
REG="${REG:?falta REG (poca|media|mucha)}"
KEY_ALG="${KEY_ALG:-rsa}"
export KEY_ALG

# cmake: highs-sys. gcc 12.3: aws-lc-sys (el gcc 10.1 del sistema arrastra el
# bug de memcmp). protoc moderno: proto3 optional. Reintentos: tras la avería
# del 2026-08-26 el árbol de módulos falla de forma intermitente en los nodos
# de cómputo, y sin esto el job muere en 3 s.
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

export DKMS_MESH_DIR="${TMPDIR:-/tmp}/dkms-certs-$SLURM_JOB_ID"
export DKMS_SCALE_OUT="$HOME/dkms_rust/certs-results/$KEY_ALG"
mkdir -p "$DKMS_SCALE_OUT"

# Certs pre-generados. Para rsa es opcional (el openssl de aquí los hace); para
# cualquier otro algoritmo es obligatorio y se comprueba que cubran este N.
CERTS_DIR="${CERTS_DIR:-$HOME/dkms_rust/certs-pregen/$KEY_ALG}"
if [ "$KEY_ALG" != rsa ]; then
    [ -s "$CERTS_DIR/net-ca.crt" ] || {
        echo "FATAL: KEY_ALG=$KEY_ALG exige certs pre-generados en $CERTS_DIR"
        echo "       (el OpenSSL de este clúster es $(openssl version) y no genera ML-DSA)"
        exit 1
    }
    [ -s "$CERTS_DIR/dkms-$N.crt" ] || {
        echo "FATAL: $CERTS_DIR no llega a N=$N (falta dkms-$N.crt)"; exit 1; }
    export DKMS_MESH_CERTS_SRC="$CERTS_DIR"
fi

echo "########## $(hostname) · $TOPO n=$N $REG · certs=$KEY_ALG · $(date '+%F %T') ##########"
echo "=== openssl del sistema: $(openssl version) ==="
if [ -n "${DKMS_MESH_CERTS_SRC:-}" ]; then
    echo "=== certs sembrados desde $DKMS_MESH_CERTS_SRC ==="
    openssl x509 -in "$DKMS_MESH_CERTS_SRC/dkms-1.crt" -noout -text 2>/dev/null \
        | grep -m1 "Signature Algorithm" || true
fi

echo "=== compilando (incremental) ==="
time cargo build --release -j "$(nproc)"

# El cliente de carga Rust es lo que hace medible el arm ML-DSA. Si no está,
# `scale_one.sh` caería al de Python y el job produciría un CSV vacío tras 20 s
# de check_load. Mejor morir aquí, con el motivo escrito.
[ -x target/release/sae_load ] || { echo "FATAL: falta target/release/sae_load"; exit 1; }
echo "=== loader: $(./target/release/sae_load --help 2>&1 | head -1) ==="

bash tests/local-mesh/scale_one.sh "$TOPO" "$N" "$REG"
echo "########## fin $(date '+%F %T') ##########"
