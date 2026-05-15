#!/usr/bin/env bash
set -euo pipefail

: "${KUBECTL:=kubectl}"
: "${NAMESPACE:=dkms-main-ns}"
: "${WEB_NAMESPACE:=web-dkms}"
: "${INGRESS_CLASS:=nginx}"
: "${INGRESS_HOST:=}"
: "${RUNTIME_INGRESS_HOST:=}"
: "${WAIT_TIMEOUT:=240s}"
: "${INGRESS_CONTROLLER_NAMESPACE:=ingress-nginx}"
: "${INGRESS_CONTROLLER_SERVICE:=ingress-nginx-controller}"
: "${INGRESS_CONTROLLER_DEPLOYMENT:=ingress-nginx-controller}"
: "${TLS_CERT_MANAGER_VERSION:=v1.19.1}"
: "${TLS_ACME_EMAIL:=}"
: "${TLS_ACME_SERVER:=https://acme-staging-v02.api.letsencrypt.org/directory}"
: "${TLS_CLUSTER_ISSUER:=letsencrypt-http01}"
: "${TLS_SECRET_NAME:=ingress-nginx-default-tls}"
: "${TLS_CERT_NAME:=ingress-nginx-default-cert}"
: "${TLS_DNS_NAME:=}"
: "${TLS_DNS_NAMES:=}"
: "${TLS_ACME_HTTP01_NAMESERVERS:=1.1.1.1:53,8.8.8.8:53}"
: "${TLS_WAIT_TIMEOUT:=900s}"
: "${TLS_CERT_WAIT_RETRIES:=2}"
: "${TLS_ALLOW_SELFSIGNED_FALLBACK:=true}"
: "${TLS_SELFSIGNED_DAYS:=30}"

declare -a TLS_DNS_NAME_LIST=()

ACTION="${1:-enable}"

if ! command -v "$KUBECTL" >/dev/null 2>&1; then
  echo "Error: no se encontro $KUBECTL" >&2
  exit 1
fi

if command -v python3 >/dev/null 2>&1; then
  PYTHON_BIN="python3"
elif command -v python >/dev/null 2>&1; then
  PYTHON_BIN="python"
else
  echo "Error: se requiere python3 o python para parchear el deployment de ingress-nginx" >&2
  exit 1
fi

resolve_ingress_host() {
  if [[ -n "$INGRESS_HOST" && "$INGRESS_HOST" != "dkms.uvigo.com" ]]; then
    :
  else
    local ctrl_host ctrl_ip
    ctrl_host="$("$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get svc "$INGRESS_CONTROLLER_SERVICE" -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)"
    ctrl_ip="$("$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get svc "$INGRESS_CONTROLLER_SERVICE" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
    if [[ -n "$ctrl_host" ]]; then
      INGRESS_HOST="$ctrl_host"
    elif [[ -n "$ctrl_ip" ]]; then
      INGRESS_HOST="$ctrl_ip"
    fi
  fi

  if [[ -z "$INGRESS_HOST" ]]; then
    echo "Error: no se pudo resolver INGRESS_HOST. Define INGRESS_HOST manualmente." >&2
    exit 1
  fi

  if [[ -z "$RUNTIME_INGRESS_HOST" ]]; then
    RUNTIME_INGRESS_HOST="runtime.${INGRESS_HOST}"
  fi

  if [[ -z "$TLS_DNS_NAME" ]]; then
    TLS_DNS_NAME="$INGRESS_HOST"
  fi

  if [[ -z "$TLS_DNS_NAMES" ]]; then
    TLS_DNS_NAMES="${TLS_DNS_NAME},${RUNTIME_INGRESS_HOST}"
  fi

  build_tls_dns_name_list
}

append_unique_dns_name() {
  local candidate
  candidate="$(printf '%s' "$1" | xargs)"
  if [[ -z "$candidate" ]]; then
    return 0
  fi

  local existing
  for existing in "${TLS_DNS_NAME_LIST[@]:-}"; do
    if [[ "$existing" == "$candidate" ]]; then
      return 0
    fi
  done
  TLS_DNS_NAME_LIST+=("$candidate")
}

build_tls_dns_name_list() {
  TLS_DNS_NAME_LIST=()

  local token
  local raw
  raw="${TLS_DNS_NAMES//,/ }"
  for token in $raw; do
    append_unique_dns_name "$token"
  done

  append_unique_dns_name "$TLS_DNS_NAME"
  append_unique_dns_name "$INGRESS_HOST"
  append_unique_dns_name "$RUNTIME_INGRESS_HOST"

  if [[ "${#TLS_DNS_NAME_LIST[@]}" -lt 1 ]]; then
    echo "Error: no hay DNS names para el certificado TLS." >&2
    exit 1
  fi

  TLS_DNS_NAME="${TLS_DNS_NAME_LIST[0]}"
  TLS_DNS_NAMES="$(IFS=,; echo "${TLS_DNS_NAME_LIST[*]}")"
}

dns_names_csv() {
  (IFS=,; echo "${TLS_DNS_NAME_LIST[*]}")
}

host_ipv4s() {
  local host="$1"
  if command -v getent >/dev/null 2>&1; then
    getent ahostsv4 "$host" 2>/dev/null | awk '{print $1}' | sort -u
    return 0
  fi
  return 1
}

preflight_dns() {
  local dns_name dns_ips ingress_ips overlap
  local ok=1

  ingress_ips="$(host_ipv4s "$INGRESS_HOST" || true)"
  for dns_name in "${TLS_DNS_NAME_LIST[@]}"; do
    dns_ips="$(host_ipv4s "$dns_name" || true)"
    if [[ -z "$dns_ips" ]]; then
      echo "Error: TLS_DNS_NAME=${dns_name} no resuelve por DNS." >&2
      ok=0
      continue
    fi

    if [[ -n "$ingress_ips" ]]; then
      overlap="$(comm -12 <(printf '%s\n' "$dns_ips" | sort -u) <(printf '%s\n' "$ingress_ips" | sort -u) || true)"
      if [[ -z "$overlap" ]]; then
        echo "[https-enable] Aviso: DNS ${dns_name} no resuelve a las mismas IPv4 que INGRESS_HOST (${INGRESS_HOST})." >&2
        echo "[https-enable] Si usas proxy/CDN (por ejemplo Cloudflare), verifica que el origen sea accesible en 80/443 para HTTP-01." >&2
      fi
    fi
  done

  if [[ "$ok" -ne 1 ]]; then
    echo "Configura el DNS público (A/CNAME) para todos los hosts antes de lanzar HTTPS." >&2
    return 1
  fi
}

install_cert_manager() {
  local manifest_url
  manifest_url="https://github.com/cert-manager/cert-manager/releases/download/${TLS_CERT_MANAGER_VERSION}/cert-manager.yaml"

  echo "[https-enable] Aplicando cert-manager ${TLS_CERT_MANAGER_VERSION}..."
  "$KUBECTL" apply -f "$manifest_url"

  echo "[https-enable] Esperando deployments de cert-manager..."
  "$KUBECTL" -n cert-manager rollout status deployment/cert-manager --timeout="$TLS_WAIT_TIMEOUT"
  "$KUBECTL" -n cert-manager rollout status deployment/cert-manager-cainjector --timeout="$TLS_WAIT_TIMEOUT"
  "$KUBECTL" -n cert-manager rollout status deployment/cert-manager-webhook --timeout="$TLS_WAIT_TIMEOUT"
}

patch_cert_manager_http01_nameservers() {
  if [[ -z "$TLS_ACME_HTTP01_NAMESERVERS" ]]; then
    echo "[https-enable] TLS_ACME_HTTP01_NAMESERVERS vacio; se omite parche de nameservers HTTP-01."
    return 0
  fi

  local desired_arg patch_payload
  desired_arg="--acme-http01-solver-nameservers=${TLS_ACME_HTTP01_NAMESERVERS}"

  patch_payload="$("$KUBECTL" -n cert-manager get deployment cert-manager -o json | \
    DESIRED_ARG="$desired_arg" "$PYTHON_BIN" -c '
import json
import os
import sys

doc = json.load(sys.stdin)
containers = doc["spec"]["template"]["spec"]["containers"]
target = None
for c in containers:
    if c.get("name") == "cert-manager":
        target = c
        break
if target is None:
    target = containers[0]

args = list(target.get("args", []))
desired = os.environ["DESIRED_ARG"]
args = [arg for arg in args if not str(arg).startswith("--acme-http01-solver-nameservers=")]
args.append(desired)

patch = {
    "spec": {
        "template": {
            "spec": {
                "containers": [
                    {
                        "name": target["name"],
                        "args": args,
                    }
                ]
            }
        }
    }
}
print(json.dumps(patch))
'
  )"

  echo "[https-enable] Forzando nameservers HTTP-01 de cert-manager: ${TLS_ACME_HTTP01_NAMESERVERS}"
  "$KUBECTL" -n cert-manager patch deployment cert-manager --type strategic -p "$patch_payload"
  "$KUBECTL" -n cert-manager rollout status deployment/cert-manager --timeout="$TLS_WAIT_TIMEOUT"
}

apply_cluster_issuer() {
  if [[ -z "$TLS_ACME_EMAIL" ]]; then
    echo "Error: TLS_ACME_EMAIL es obligatorio para configurar ACME/Let's Encrypt." >&2
    return 1
  fi

  echo "[https-enable] Aplicando ClusterIssuer ${TLS_CLUSTER_ISSUER}..."
  cat <<EOF | "$KUBECTL" apply -f -
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: ${TLS_CLUSTER_ISSUER}
spec:
  acme:
    email: ${TLS_ACME_EMAIL}
    server: ${TLS_ACME_SERVER}
    privateKeySecretRef:
      name: ${TLS_CLUSTER_ISSUER}-account-key
    solvers:
    - http01:
        ingress:
          ingressClassName: ${INGRESS_CLASS}
EOF

  if ! "$KUBECTL" wait --for=condition=Ready "clusterissuer/${TLS_CLUSTER_ISSUER}" --timeout="$TLS_WAIT_TIMEOUT"; then
    echo "Error: ClusterIssuer ${TLS_CLUSTER_ISSUER} no quedo Ready." >&2
    "$KUBECTL" describe "clusterissuer/${TLS_CLUSTER_ISSUER}" || true
    return 1
  fi
}

apply_certificate() {
  local retries dns_names_yaml dns_name
  retries="$TLS_CERT_WAIT_RETRIES"
  if ! [[ "$retries" =~ ^[1-9][0-9]*$ ]]; then
    retries=1
  fi

  dns_names_yaml=""
  for dns_name in "${TLS_DNS_NAME_LIST[@]}"; do
    dns_names_yaml="${dns_names_yaml}  - ${dns_name}"$'\n'
  done

  echo "[https-enable] Aplicando Certificate ${TLS_CERT_NAME} (dns=$(dns_names_csv))..."
  cat <<EOF | "$KUBECTL" apply -f -
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: ${TLS_CERT_NAME}
  namespace: ${INGRESS_CONTROLLER_NAMESPACE}
spec:
  secretName: ${TLS_SECRET_NAME}
  issuerRef:
    name: ${TLS_CLUSTER_ISSUER}
    kind: ClusterIssuer
  dnsNames:
${dns_names_yaml}
EOF

  for attempt in $(seq 1 "$retries"); do
    if "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" wait --for=condition=Ready "certificate/${TLS_CERT_NAME}" --timeout="$TLS_WAIT_TIMEOUT"; then
      return 0
    fi

    echo "[https-enable] Aviso: Certificate ${TLS_CERT_NAME} no quedó Ready (intento ${attempt}/${retries})." >&2
    "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get "certificate/${TLS_CERT_NAME}" -o wide || true
    "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get certificaterequest,order,challenge || true
    if [[ "$attempt" -lt "$retries" ]]; then
      echo "[https-enable] Reintentando espera del certificado..." >&2
    fi
  done

  echo "Error: Certificate ${TLS_CERT_NAME} no quedo Ready tras ${retries} intento(s)." >&2
  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" describe "certificate/${TLS_CERT_NAME}" || true
  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get certificaterequest,order,challenge || true
  echo "Nota: si ACME devuelve 'rejectedIdentifier' para un hostname '*.elb.*.amazonaws.com', esa CA no permite emitir para ese dominio gestionado por AWS." >&2
  echo "Usa un dominio propio (CNAME al ELB) o una CA/ACME que acepte ese hostname." >&2
  return 1
}

patch_default_ssl_certificate_arg() {
  local desired_arg patch_payload
  desired_arg="--default-ssl-certificate=${INGRESS_CONTROLLER_NAMESPACE}/${TLS_SECRET_NAME}"

  patch_payload="$("$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get deployment "$INGRESS_CONTROLLER_DEPLOYMENT" -o json | \
    DESIRED_ARG="$desired_arg" "$PYTHON_BIN" -c '
import json
import os
import sys

doc = json.load(sys.stdin)
containers = doc["spec"]["template"]["spec"]["containers"]
container_name = None
args = None
for c in containers:
    if c.get("name") == "controller":
        container_name = c["name"]
        args = list(c.get("args", []))
        break
if container_name is None:
    container_name = containers[0]["name"]
    args = list(containers[0].get("args", []))

desired = os.environ["DESIRED_ARG"]
args = [arg for arg in args if not str(arg).startswith("--default-ssl-certificate=")]
args.append(desired)
patch = {
    "spec": {
        "template": {
            "spec": {
                "containers": [
                    {
                        "name": container_name,
                        "args": args,
                    }
                ]
            }
        }
    }
}
print(json.dumps(patch))
'
  )"

  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" patch deployment "$INGRESS_CONTROLLER_DEPLOYMENT" \
    --type strategic \
    -p "$patch_payload"
}

apply_ingress_default_cert() {
  local deploy_patch_out configmap_patch_out needs_restart
  needs_restart=0

  echo "[https-enable] Configurando certificado por defecto en ingress-nginx..."
  deploy_patch_out="$(patch_default_ssl_certificate_arg)"
  echo "$deploy_patch_out"
  if [[ "$deploy_patch_out" != *"(no change)"* ]]; then
    needs_restart=1
  fi

  echo "[https-enable] Activando redireccion global a HTTPS..."
  configmap_patch_out="$("$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" patch configmap ingress-nginx-controller \
    --type merge \
    -p '{"data":{"force-ssl-redirect":"true","no-tls-redirect-locations":"/.well-known/acme-challenge","ssl-protocols":"TLSv1.2 TLSv1.3"}}')"
  echo "$configmap_patch_out"
  if [[ "$configmap_patch_out" != *"(no change)"* ]]; then
    needs_restart=1
  fi

  if [[ "$needs_restart" -eq 1 ]]; then
    echo "[https-enable] Reiniciando ingress-nginx-controller para aplicar cambios..."
    "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" rollout restart "deployment/${INGRESS_CONTROLLER_DEPLOYMENT}"
    if ! "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" rollout status "deployment/${INGRESS_CONTROLLER_DEPLOYMENT}" --timeout="$WAIT_TIMEOUT"; then
      echo "[https-enable] Aviso: rollout de ingress-nginx no alcanzó estado completo; se valida disponibilidad mínima por endpoints..." >&2
    fi
  else
    echo "[https-enable] Sin cambios efectivos en ingress-nginx; se omite restart."
  fi
  wait_for_ingress_endpoints_ready
}

apply_selfsigned_certificate() {
  local days tmp_dir cert_file key_file san dns_name primary_dns
  local san_entries=()
  days="$TLS_SELFSIGNED_DAYS"
  if ! [[ "$days" =~ ^[1-9][0-9]*$ ]]; then
    days=30
  fi
  if ! command -v openssl >/dev/null 2>&1; then
    echo "Error: openssl no está instalado y no se puede generar certificado self-signed." >&2
    exit 1
  fi

  tmp_dir="$(mktemp -d)"
  cert_file="$tmp_dir/tls.crt"
  key_file="$tmp_dir/tls.key"
  primary_dns="${TLS_DNS_NAME_LIST[0]}"
  for dns_name in "${TLS_DNS_NAME_LIST[@]}"; do
    san_entries+=("DNS:${dns_name}")
  done
  san="subjectAltName=$(IFS=,; echo "${san_entries[*]}")"

  echo "[https-enable] Generando certificado self-signed para $(dns_names_csv) (${days} días)..."
  openssl req -x509 -nodes -newkey rsa:2048 -sha256 \
    -days "$days" \
    -subj "/CN=${primary_dns}" \
    -addext "$san" \
    -keyout "$key_file" \
    -out "$cert_file" >/dev/null 2>&1

  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" create secret tls "$TLS_SECRET_NAME" \
    --cert="$cert_file" \
    --key="$key_file" \
    --dry-run=client -o yaml | "$KUBECTL" apply -f -

  rm -rf "$tmp_dir"
}

wait_for_ingress_endpoints_ready() {
  local max_attempts attempt ready_ips ready_count
  max_attempts=60

  for attempt in $(seq 1 "$max_attempts"); do
    ready_ips="$("$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get endpoints "$INGRESS_CONTROLLER_SERVICE" \
      -o jsonpath='{range .subsets[*].addresses[*]}{.ip}{"\n"}{end}' 2>/dev/null || true)"
    ready_count="$(printf '%s\n' "$ready_ips" | awk 'NF{count++} END{print count+0}')"
    if [[ "$ready_count" -ge 1 ]]; then
      echo "[https-enable] ingress-nginx disponible: ${ready_count} endpoint(s) Ready."
      return 0
    fi
    sleep 5
  done

  echo "Error: ingress-nginx no expone endpoints Ready tras esperar disponibilidad mínima." >&2
  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get pods -l app.kubernetes.io/component=controller -o wide || true
  "$KUBECTL" -n "$INGRESS_CONTROLLER_NAMESPACE" get endpoints "$INGRESS_CONTROLLER_SERVICE" -o wide || true
  return 1
}

patch_ingress_host() {
  local ns="$1"
  local ingress_name="$2"
  local ingress_host="$3"
  if [[ -z "$ingress_host" ]]; then
    return 0
  fi
  "$KUBECTL" -n "$ns" patch ingress "$ingress_name" \
    --type json \
    -p "[{\"op\":\"add\",\"path\":\"/spec/rules/0/host\",\"value\":\"${ingress_host}\"}]"
  echo "[https-enable] Host sincronizado: ${ns}/${ingress_name} -> ${ingress_host}"
}

sync_namespace_ingress_hosts() {
  local ns="$1"
  local ingresses
  ingresses="$("$KUBECTL" -n "$ns" get ingress -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true)"
  if [[ -z "$ingresses" ]]; then
    return 0
  fi

  while IFS= read -r ingress_name; do
    if [[ -z "$ingress_name" ]]; then
      continue
    fi
    if [[ "$ingress_name" == *"-runtime-ingress" ]]; then
      patch_ingress_host "$ns" "$ingress_name" "$RUNTIME_INGRESS_HOST"
    else
      patch_ingress_host "$ns" "$ingress_name" "$INGRESS_HOST"
    fi
  done <<< "$ingresses"
}

sync_ingress_hosts() {
  local namespaces sim_ns
  namespaces=("$NAMESPACE" "$WEB_NAMESPACE")
  while IFS= read -r sim_ns; do
    if [[ -n "$sim_ns" ]]; then
      namespaces+=("$sim_ns")
    fi
  done < <("$KUBECTL" get namespaces -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | awk '/^[0-9]+$/ {print}')

  echo "[https-enable] Sincronizando hosts en ingress (gestion=${INGRESS_HOST}, runtime=${RUNTIME_INGRESS_HOST})..."
  for ns in "${namespaces[@]}"; do
    if "$KUBECTL" get namespace "$ns" >/dev/null 2>&1; then
      sync_namespace_ingress_hosts "$ns"
    fi
  done
}

run_enable() {
  resolve_ingress_host || return 1
  preflight_dns || return 1
  echo "[https-enable] INGRESS_HOST=${INGRESS_HOST}"
  echo "[https-enable] RUNTIME_INGRESS_HOST=${RUNTIME_INGRESS_HOST}"
  echo "[https-enable] TLS_DNS_NAME=${TLS_DNS_NAME}"
  echo "[https-enable] TLS_DNS_NAMES=$(dns_names_csv)"
  echo "[https-enable] TLS_ACME_SERVER=${TLS_ACME_SERVER}"

  install_cert_manager || return 1
  patch_cert_manager_http01_nameservers || return 1
  apply_cluster_issuer || return 1
  apply_certificate || return 1
  apply_ingress_default_cert || return 1
  sync_ingress_hosts || return 1

  echo "[https-enable] HTTPS habilitado correctamente."
}

run_selfsigned_fallback() {
  resolve_ingress_host
  echo "[https-enable] Activando fallback TLS self-signed..."
  apply_selfsigned_certificate
  apply_ingress_default_cert
  sync_ingress_hosts
  echo "[https-enable] HTTPS habilitado con certificado self-signed."
}

case "$ACTION" in
  cert-manager-install)
    install_cert_manager
    patch_cert_manager_http01_nameservers
    ;;
  tls-issuer-apply)
    resolve_ingress_host
    apply_cluster_issuer
    ;;
  tls-certificate-apply)
    resolve_ingress_host
    apply_certificate
    ;;
  ingress-default-cert-apply)
    resolve_ingress_host
    apply_ingress_default_cert
    ;;
  ingress-sync-host)
    resolve_ingress_host
    sync_ingress_hosts
    ;;
  selfsigned-fallback)
    run_selfsigned_fallback
    ;;
  enable)
    if run_enable; then
      :
    elif [[ "${TLS_ALLOW_SELFSIGNED_FALLBACK}" == "true" ]]; then
      echo "[https-enable] Aviso: activación ACME fallida; se aplica fallback self-signed." >&2
      run_selfsigned_fallback
    else
      exit 1
    fi
    ;;
  *)
    echo "Uso: $0 [enable|cert-manager-install|tls-issuer-apply|tls-certificate-apply|ingress-default-cert-apply|ingress-sync-host|selfsigned-fallback]" >&2
    exit 2
    ;;
esac
