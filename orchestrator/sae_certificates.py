from __future__ import annotations

import os
import re
import base64
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Iterable, Mapping, Optional
from urllib.parse import unquote

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa
from cryptography.hazmat.primitives.serialization import pkcs12
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID


SAN_URI_PREFIX = os.getenv("SAE_SAN_URI_PREFIX", "urn:dkms:sae:")
DEFAULT_CERT_DAYS = int(os.getenv("SAE_CERT_DAYS", "90"))


@dataclass(frozen=True)
class IssuedCertificate:
    certificate_pem: str
    ca_chain_pem: str
    private_key_pem: Optional[str]
    serial_hex: str
    fingerprint_sha256: str
    subject_rfc4514: str
    not_before: datetime
    not_after: datetime


_CA_CACHE: tuple[x509.Certificate, rsa.RSAPrivateKey | ec.EllipticCurvePrivateKey] | None = None
PrivateKeyType = rsa.RSAPrivateKey | ec.EllipticCurvePrivateKey


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _read_pem_from_env(raw_env: str, path_env: str) -> Optional[str]:
    raw = (os.getenv(raw_env) or "").strip()
    if raw:
        return raw.replace("\\n", "\n")

    raw_path = (os.getenv(path_env) or "").strip()
    if raw_path:
        path = Path(raw_path)
        if path.is_file():
            return path.read_text(encoding="utf-8")
    return None


def _build_ephemeral_ca() -> tuple[x509.Certificate, rsa.RSAPrivateKey]:
    now = _utc_now()
    key = rsa.generate_private_key(public_exponent=65537, key_size=3072)
    subject = x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "ES"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "DKMS Runtime CA"),
            x509.NameAttribute(NameOID.COMMON_NAME, "dkms-runtime-ca"),
        ]
    )
    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(minutes=5))
        .not_valid_after(now + timedelta(days=3650))
        .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=False,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                key_cert_sign=True,
                crl_sign=True,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .sign(private_key=key, algorithm=hashes.SHA256())
    )
    return cert, key


def _load_ca_material_from_pem(
    *,
    cert_pem: str,
    key_pem: str,
    key_password: Optional[str] = None,
) -> tuple[x509.Certificate, PrivateKeyType]:
    cert = x509.load_pem_x509_certificate(cert_pem.encode("utf-8"))
    key = serialization.load_pem_private_key(
        key_pem.encode("utf-8"),
        password=(key_password or "").encode("utf-8") or None,
    )
    if not isinstance(key, (rsa.RSAPrivateKey, ec.EllipticCurvePrivateKey)):
        raise RuntimeError("SAE CA key type is not supported")
    return cert, key


def _load_ca_material() -> tuple[x509.Certificate, PrivateKeyType]:
    global _CA_CACHE
    if _CA_CACHE is not None:
        return _CA_CACHE

    cert_pem = _read_pem_from_env("SAE_CA_CERT_PEM", "SAE_CA_CERT_PATH")
    key_pem = _read_pem_from_env("SAE_CA_KEY_PEM", "SAE_CA_KEY_PATH")

    if cert_pem and key_pem:
        cert, key = _load_ca_material_from_pem(
            cert_pem=cert_pem,
            key_pem=key_pem,
            key_password=os.getenv("SAE_CA_KEY_PASSWORD"),
        )
        _CA_CACHE = (cert, key)
        return _CA_CACHE

    _CA_CACHE = _build_ephemeral_ca()
    return _CA_CACHE


def _resolve_ca_material(
    *,
    ca_certificate_pem: Optional[str] = None,
    ca_private_key_pem: Optional[str] = None,
    ca_private_key_password: Optional[str] = None,
) -> tuple[x509.Certificate, PrivateKeyType]:
    has_cert = bool(str(ca_certificate_pem or "").strip())
    has_key = bool(str(ca_private_key_pem or "").strip())
    if has_cert != has_key:
        raise ValueError("Both ca_certificate_pem and ca_private_key_pem are required together")
    if has_cert and has_key:
        return _load_ca_material_from_pem(
            cert_pem=str(ca_certificate_pem),
            key_pem=str(ca_private_key_pem),
            key_password=ca_private_key_password,
        )
    return _load_ca_material()


def _sae_subject(sae_id: str) -> x509.Name:
    value = str(sae_id).strip()
    return x509.Name(
        [
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "DKMS SAE"),
            x509.NameAttribute(NameOID.COMMON_NAME, value),
        ]
    )


def _required_san_uri(sae_id: str) -> str:
    return f"{SAN_URI_PREFIX}{str(sae_id).strip()}"


def _merge_san_entries(existing: Iterable[x509.GeneralName], sae_id: str) -> x509.SubjectAlternativeName:
    required_uri = _required_san_uri(sae_id)
    merged: list[x509.GeneralName] = []
    seen: set[str] = set()

    for entry in existing:
        serialized = repr(entry)
        if serialized in seen:
            continue
        seen.add(serialized)
        merged.append(entry)

    uri_entries = {
        entry.value
        for entry in merged
        if isinstance(entry, x509.UniformResourceIdentifier)
    }
    if required_uri not in uri_entries:
        merged.append(x509.UniformResourceIdentifier(required_uri))

    return x509.SubjectAlternativeName(merged)


def _build_leaf_builder(
    *,
    sae_id: str,
    public_key,
    days_valid: int,
    ca_cert: x509.Certificate,
) -> x509.CertificateBuilder:
    now = _utc_now()
    days = max(1, int(days_valid))

    return (
        x509.CertificateBuilder()
        .issuer_name(ca_cert.subject)
        .subject_name(_sae_subject(sae_id))
        .public_key(public_key)
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(minutes=5))
        .not_valid_after(now + timedelta(days=days))
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=True,
                content_commitment=False,
                key_encipherment=True,
                data_encipherment=False,
                key_agreement=True,
                key_cert_sign=False,
                crl_sign=False,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .add_extension(
            x509.ExtendedKeyUsage([ExtendedKeyUsageOID.CLIENT_AUTH]),
            critical=False,
        )
    )


def _encode_cert_bundle(
    *,
    cert: x509.Certificate,
    ca_cert: x509.Certificate,
    private_key=None,
) -> IssuedCertificate:
    cert_pem = cert.public_bytes(serialization.Encoding.PEM).decode("utf-8")
    ca_pem = ca_cert.public_bytes(serialization.Encoding.PEM).decode("utf-8")
    private_key_pem = None
    if private_key is not None:
        private_key_pem = private_key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption(),
        ).decode("utf-8")

    return IssuedCertificate(
        certificate_pem=cert_pem,
        ca_chain_pem=ca_pem,
        private_key_pem=private_key_pem,
        serial_hex=f"{cert.serial_number:x}",
        fingerprint_sha256=cert.fingerprint(hashes.SHA256()).hex(),
        subject_rfc4514=cert.subject.rfc4514_string(),
        not_before=cert.not_valid_before_utc,
        not_after=cert.not_valid_after_utc,
    )


def issue_sae_certificate(
    sae_id: str,
    *,
    key_type: str = "ec-p256",
    days_valid: int = DEFAULT_CERT_DAYS,
    ca_certificate_pem: Optional[str] = None,
    ca_private_key_pem: Optional[str] = None,
    ca_private_key_password: Optional[str] = None,
) -> IssuedCertificate:
    key_type_normalized = str(key_type or "").strip().lower()
    if key_type_normalized == "rsa-2048":
        private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    else:
        private_key = ec.generate_private_key(ec.SECP256R1())

    ca_cert, ca_key = _resolve_ca_material(
        ca_certificate_pem=ca_certificate_pem,
        ca_private_key_pem=ca_private_key_pem,
        ca_private_key_password=ca_private_key_password,
    )

    builder = _build_leaf_builder(
        sae_id=sae_id,
        public_key=private_key.public_key(),
        days_valid=days_valid,
        ca_cert=ca_cert,
    )
    builder = builder.add_extension(
        _merge_san_entries([], sae_id),
        critical=False,
    )

    cert = builder.sign(private_key=ca_key, algorithm=hashes.SHA256())
    return _encode_cert_bundle(cert=cert, ca_cert=ca_cert, private_key=private_key)


def sign_sae_csr(
    sae_id: str,
    *,
    csr_pem: str,
    days_valid: int = DEFAULT_CERT_DAYS,
    ca_certificate_pem: Optional[str] = None,
    ca_private_key_pem: Optional[str] = None,
    ca_private_key_password: Optional[str] = None,
) -> IssuedCertificate:
    csr = x509.load_pem_x509_csr(csr_pem.encode("utf-8"))
    try:
        if hasattr(csr, "is_signature_valid") and not csr.is_signature_valid:
            raise ValueError("CSR signature is invalid")
    except Exception as exc:  # pragma: no cover - defensive
        raise ValueError("CSR signature is invalid") from exc

    ca_cert, ca_key = _resolve_ca_material(
        ca_certificate_pem=ca_certificate_pem,
        ca_private_key_pem=ca_private_key_pem,
        ca_private_key_password=ca_private_key_password,
    )

    builder = _build_leaf_builder(
        sae_id=sae_id,
        public_key=csr.public_key(),
        days_valid=days_valid,
        ca_cert=ca_cert,
    )

    san_entries: list[x509.GeneralName] = []
    try:
        ext = csr.extensions.get_extension_for_class(x509.SubjectAlternativeName)
        san_entries = list(ext.value)
    except x509.ExtensionNotFound:
        san_entries = []

    builder = builder.add_extension(_merge_san_entries(san_entries, sae_id), critical=False)

    cert = builder.sign(private_key=ca_key, algorithm=hashes.SHA256())
    return _encode_cert_bundle(cert=cert, ca_cert=ca_cert)


def bundle_to_pkcs12_base64(
    *,
    certificate_pem: str,
    private_key_pem: str,
    ca_chain_pem: str,
    password: Optional[str] = None,
) -> str:
    cert = x509.load_pem_x509_certificate(certificate_pem.encode("utf-8"))
    key = serialization.load_pem_private_key(private_key_pem.encode("utf-8"), password=None)
    ca_cert = x509.load_pem_x509_certificate(ca_chain_pem.encode("utf-8"))

    encryption = serialization.NoEncryption()
    if password:
        encryption = serialization.BestAvailableEncryption(password.encode("utf-8"))

    data = pkcs12.serialize_key_and_certificates(
        name=b"sae",
        key=key,
        cert=cert,
        cas=[ca_cert],
        encryption_algorithm=encryption,
    )
    return base64.b64encode(data).decode("ascii")


def _decode_forwarded_cert(raw: str) -> Optional[str]:
    value = str(raw or "").strip()
    if not value:
        return None

    # Some proxies forward x-forwarded-client-cert as a structured header.
    structured_match = re.search(r"Cert=\"([^\"]+)\"", value)
    if structured_match:
        value = structured_match.group(1)

    value = unquote(value)
    if "BEGIN CERTIFICATE" not in value:
        return None

    if "-----BEGIN CERTIFICATE-----" in value:
        return value

    # Tolerate BEGIN/END without PEM line formatting.
    begin = value.find("BEGIN CERTIFICATE")
    end = value.find("END CERTIFICATE")
    if begin == -1 or end == -1:
        return None
    body = value[begin + len("BEGIN CERTIFICATE") : end].replace("-", "").strip()
    lines = [body[i : i + 64] for i in range(0, len(body), 64)]
    pem = "-----BEGIN CERTIFICATE-----\n" + "\n".join(lines) + "\n-----END CERTIFICATE-----\n"
    return pem


def extract_client_certificate_from_headers(headers: Mapping[str, str]) -> Optional[x509.Certificate]:
    candidates = [
        "ssl-client-cert",
        "x-ssl-client-cert",
        "x-forwarded-tls-client-cert",
        "x-client-cert",
        "x-forwarded-client-cert",
    ]
    for key in candidates:
        raw = headers.get(key) or headers.get(key.upper()) or headers.get(key.title())
        if not raw:
            continue
        pem = _decode_forwarded_cert(raw)
        if not pem:
            continue
        try:
            return x509.load_pem_x509_certificate(pem.encode("utf-8"))
        except ValueError:
            continue
    return None


def sae_id_from_certificate(cert: x509.Certificate) -> Optional[str]:
    try:
        san_ext = cert.extensions.get_extension_for_class(x509.SubjectAlternativeName)
    except x509.ExtensionNotFound:
        return None

    for uri in san_ext.value.get_values_for_type(x509.UniformResourceIdentifier):
        text = str(uri or "").strip()
        if not text.startswith(SAN_URI_PREFIX):
            continue
        sae_id = text[len(SAN_URI_PREFIX) :].strip()
        if sae_id:
            return sae_id
    return None


def fingerprint_sha256(cert: x509.Certificate) -> str:
    return cert.fingerprint(hashes.SHA256()).hex()


def serial_hex(cert: x509.Certificate) -> str:
    return f"{cert.serial_number:x}"
