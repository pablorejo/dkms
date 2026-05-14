#!/usr/bin/env python3
"""
Llama a un DKMS via Ingress usando Bearer (AuthZ) o mTLS.

Ejemplo:
  python code_dkms/src/k8s/test/request_dkms_ingress.py \
    --runtime-base-url "https://<RUNTIME_INGRESS_HOST>" \
    --base-url "https://<INGRESS_HOST>" \
    --sim-id 1 \
    --dkms-id 13 \
    --path "/api/v1/keys/status" \
    --auth-mode mtls \
    --mtls-cert ./sae.client.crt.pem \
    --mtls-key ./sae.client.key.pem \
    --mtls-ca ./sae.ca.crt.pem
"""

from __future__ import annotations

import argparse
import sys
from typing import Optional

import requests


def _normalize_path(path: str) -> str:
    if not path:
        return ""
    if not path.startswith("/"):
        return f"/{path}"
    return path


def _build_url(base_url: str, sim_id: int, dkms_id: int, path: str) -> str:
    base = base_url.rstrip("/")
    suffix = _normalize_path(path)
    return f"{base}/api/sim/{sim_id}/dkms/{dkms_id}{suffix}"


def _login_for_token(
    base_url: str,
    username: Optional[str],
    email: Optional[str],
    password: str,
    timeout: int,
    insecure: bool,
) -> str:
    base = base_url.rstrip("/")
    url = f"{base}/login"
    payload: dict = {"password": password}
    if username:
        payload["username"] = username
    if email:
        payload["email"] = email
    response = requests.post(
        url,
        json=payload,
        timeout=timeout,
        verify=not insecure,
    )
    if response.status_code != 200:
        raise RuntimeError(f"Login failed ({response.status_code}): {response.text}")
    data = response.json()
    token = data.get("access_token")
    if not token:
        raise RuntimeError("Login response missing access_token")
    return token


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Llama a un DKMS por Ingress con Bearer o mTLS."
    )
    parser.add_argument("--base-url", required=True, help="URL base del Ingress")
    parser.add_argument(
        "--runtime-base-url",
        default="",
        help="URL base del runtime Ingress (si se omite usa --base-url)",
    )
    parser.add_argument(
        "--auth-mode",
        choices=("mtls", "bearer"),
        default="mtls",
        help="Modo de autenticacion para el endpoint runtime (default: mtls)",
    )
    parser.add_argument("--authz-url", help="URL base de AuthZ (default: base-url)")
    parser.add_argument("--sim-id", type=int, default=1, help="ID de simulacion")
    parser.add_argument("--dkms-id", type=int, default=13, help="ID de DKMS")
    parser.add_argument(
        "--path",
        default="/api/v1/keys/status",
        help="Path del endpoint en DKMS (default: /api/v1/keys/status)",
    )
    parser.add_argument("--username", default="config_user", help="Username para login")
    parser.add_argument("--email", help="Email para login (si no hay username)")
    parser.add_argument(
        "--password",
        default="config_password",
        help="Password para login (default: config_password)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=15,
        help="Timeout de la peticion en segundos (default: 15)",
    )
    parser.add_argument(
        "--insecure",
        action="store_true",
        help="Desactiva verificacion TLS (si usas https con cert no confiable)",
    )
    parser.add_argument("--mtls-cert", help="Ruta al certificado cliente PEM para mTLS")
    parser.add_argument("--mtls-key", help="Ruta a la clave privada PEM para mTLS")
    parser.add_argument("--mtls-ca", help="Ruta al CA PEM para verificar servidor en mTLS")
    args = parser.parse_args()

    runtime_base_url = args.runtime_base_url or args.base_url
    url = _build_url(runtime_base_url, args.sim_id, args.dkms_id, args.path)
    verify: bool | str = not args.insecure
    headers: dict[str, str] = {}
    cert: tuple[str, str] | None = None

    if args.auth_mode == "bearer":
        authz_base = args.authz_url or args.base_url
        try:
            token = _login_for_token(
                authz_base,
                args.username,
                args.email,
                args.password,
                args.timeout,
                args.insecure,
            )
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr)
            return 2
        headers = {"Authorization": f"Bearer {token}"}
    else:
        if not args.mtls_cert or not args.mtls_key:
            print("Error: --mtls-cert y --mtls-key son obligatorios con --auth-mode mtls", file=sys.stderr)
            return 2
        cert = (args.mtls_cert, args.mtls_key)
        if args.mtls_ca:
            verify = args.mtls_ca

    try:
        response = requests.get(
            url,
            headers=headers,
            timeout=args.timeout,
            verify=verify,
            cert=cert,
        )
    except requests.RequestException as exc:
        print(f"Error en la peticion: {exc}", file=sys.stderr)
        return 1

    print(f"URL: {url}")
    print(f"Status: {response.status_code}")
    print("Headers:")
    for key, value in response.headers.items():
        print(f"{key}: {value}")
    print("Body:")
    print(response.text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
