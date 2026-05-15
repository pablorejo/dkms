from __future__ import annotations

import hashlib
import hmac
import os
import re
import secrets
import time
from typing import Any, Dict, Optional
from urllib.parse import urlparse

import jwt
from jwt import InvalidTokenError
from fastapi import FastAPI, HTTPException, Request, Response
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from dotenv import load_dotenv
from sqlalchemy.exc import OperationalError

from models import ModelUser
from persistence import build_uow_from_env

load_dotenv()

JWT_SECRET = os.getenv("JWT_SECRET")
JWT_PRIVATE_KEY = os.getenv("JWT_PRIVATE_KEY")
JWT_PUBLIC_KEY = os.getenv("JWT_PUBLIC_KEY")
JWT_ALGORITHMS = [
    alg.strip()
    for alg in os.getenv("JWT_ALGORITHMS", "HS256").split(",")
    if alg.strip()
]
JWT_AUDIENCE = os.getenv("JWT_AUDIENCE")
JWT_ISSUER = os.getenv("JWT_ISSUER")
JWT_USER_CLAIM = os.getenv("JWT_USER_CLAIM", "sub")
JWT_LEEWAY = int(os.getenv("JWT_LEEWAY", "0"))
AUTHZ_TOKEN_TTL = int(os.getenv("AUTHZ_TOKEN_TTL", "3600"))

SIM_ID_REGEX = os.getenv(
    "AUTHZ_SIM_ID_REGEX",
    r"/(?:api/sim|orch/api/sim|orch/web/simulations)/(?P<sim_id>\d+)",
)
SIM_ID_HEADER = os.getenv("AUTHZ_SIM_ID_HEADER")
REQUIRE_ACTIVE_USER = os.getenv("AUTHZ_REQUIRE_ACTIVE", "true").lower() in {"1", "true", "yes"}
AUTHZ_USER_ONLY_PATHS = [
    value.strip()
    for value in os.getenv(
        "AUTHZ_USER_ONLY_PATHS",
        "/orch/health,/orch/simulations,/orch/web/simulations,/orch/admin/saes,/orch/admin/saes/*",
    ).split(",")
    if value.strip()
]
AUTHZ_USER_ONLY_METHODS = {
    value.strip().upper()
    for value in os.getenv("AUTHZ_USER_ONLY_METHODS", "GET,POST,HEAD,DELETE").split(",")
    if value.strip()
}

_SIM_ID_RE = re.compile(SIM_ID_REGEX)


def _normalize_key(key: Optional[str]) -> Optional[str]:
    if not key:
        return None
    return key.replace("\\n", "\n")


def _select_jwt_key() -> str:
    if not JWT_ALGORITHMS:
        raise RuntimeError("JWT_ALGORITHMS must not be empty")
    wants_hs = any(alg.upper().startswith("HS") for alg in JWT_ALGORITHMS)
    if wants_hs:
        secret = _normalize_key(JWT_SECRET)
        if not secret:
            raise RuntimeError("JWT_SECRET is required for HS* algorithms")
        return secret
    public_key = _normalize_key(JWT_PUBLIC_KEY)
    if not public_key:
        raise RuntimeError("JWT_PUBLIC_KEY is required for RS*/ES* algorithms")
    return public_key


def _select_signing_key(algorithm: str) -> str:
    if algorithm.upper().startswith("HS"):
        secret = _normalize_key(JWT_SECRET)
        if not secret:
            raise RuntimeError("JWT_SECRET is required for HS* algorithms")
        return secret
    private_key = _normalize_key(JWT_PRIVATE_KEY)
    if not private_key:
        raise RuntimeError("JWT_PRIVATE_KEY is required for RS*/ES* algorithms")
    return private_key


def _decode_jwt(token: str) -> Dict[str, Any]:
    key = _select_jwt_key()
    options = {
        "verify_aud": bool(JWT_AUDIENCE),
        "verify_iss": bool(JWT_ISSUER),
    }
    return jwt.decode(
        token,
        key=key,
        algorithms=JWT_ALGORITHMS,
        audience=JWT_AUDIENCE,
        issuer=JWT_ISSUER,
        options=options,
        leeway=JWT_LEEWAY,
    )


def _extract_bearer(auth_header: Optional[str]) -> str:
    if not auth_header:
        raise HTTPException(status_code=401, detail="Missing Authorization header")
    parts = auth_header.split()
    if len(parts) == 2 and parts[0].lower() == "bearer":
        return parts[1]
    return auth_header.strip()


def _get_claim(payload: Dict[str, Any], path: str) -> Optional[Any]:
    value: Any = payload
    for part in path.split("."):
        if not isinstance(value, dict) or part not in value:
            return None
        value = value[part]
    return value


def _request_path(request: Request) -> str:
    raw_path = (
        request.headers.get("x-original-uri")
        or request.headers.get("x-forwarded-uri")
        or request.headers.get("x-original-url")
        or request.url.path
        or ""
    )
    parsed = urlparse(raw_path)
    if parsed.path:
        return parsed.path
    return raw_path.split("?", 1)[0]


def _path_matches_user_only(path: str, configured_path: str) -> bool:
    normalized_path = path.rstrip("/") or "/"
    normalized_configured = configured_path.rstrip("/") or "/"
    if not normalized_configured:
        return False
    if normalized_configured.endswith("/*"):
        prefix = normalized_configured[:-2] or "/"
        return normalized_path == prefix or normalized_path.startswith(f"{prefix}/")
    return normalized_path == normalized_configured


def _allows_user_only_access(request: Request) -> bool:
    method = (
        request.headers.get("x-original-method")
        or request.headers.get("x-forwarded-method")
        or request.method
    ).upper()
    if method not in AUTHZ_USER_ONLY_METHODS:
        return False
    path = _request_path(request)
    return any(_path_matches_user_only(path, configured) for configured in AUTHZ_USER_ONLY_PATHS)


def _extract_sim_id(request: Request) -> Optional[int]:
    if SIM_ID_HEADER:
        header_val = request.headers.get(SIM_ID_HEADER)
        if header_val and header_val.isdigit():
            return int(header_val)

    query_val = request.query_params.get("sim_id")
    if query_val and query_val.isdigit():
        return int(query_val)

    path = _request_path(request)
    if not path:
        return None

    match = _SIM_ID_RE.search(path)
    if not match:
        return None
    return int(match.group("sim_id"))


def _hash_sha256(value: str, salt: str = "") -> str:
    return hashlib.sha256(f"{salt}{value}".encode("utf-8")).hexdigest()


def _hash_password(plain: str) -> str:
    salt = secrets.token_hex(16)
    digest = _hash_sha256(plain, salt)
    return f"sha256${salt}${digest}"


def _verify_password(plain: str, stored_hash: str) -> bool:
    if hmac.compare_digest(stored_hash, plain):
        return True
    if stored_hash.startswith("sha256$"):
        parts = stored_hash.split("$", 2)
        if len(parts) == 3:
            _, salt, digest = parts
            return hmac.compare_digest(_hash_sha256(plain, salt), digest)
    if len(stored_hash) == 64 and all(c in "0123456789abcdef" for c in stored_hash.lower()):
        return hmac.compare_digest(_hash_sha256(plain), stored_hash.lower())
    return False


def _encode_jwt(user_id: int) -> str:
    if not JWT_ALGORITHMS:
        raise RuntimeError("JWT_ALGORITHMS must not be empty")
    algorithm = JWT_ALGORITHMS[0]
    key = _select_signing_key(algorithm)
    now = int(time.time())
    payload: Dict[str, Any] = {
        JWT_USER_CLAIM: user_id,
        "iat": now,
        "exp": now + AUTHZ_TOKEN_TTL,
    }
    if JWT_ISSUER:
        payload["iss"] = JWT_ISSUER
    if JWT_AUDIENCE:
        payload["aud"] = JWT_AUDIENCE
    token = jwt.encode(payload, key=key, algorithm=algorithm)
    if isinstance(token, bytes):
        return token.decode("utf-8")
    return token


def _parse_port(raw: str) -> int:
    try:
        return int(raw)
    except (TypeError, ValueError):
        pass

    try:
        parsed = urlparse(raw)
        if parsed.port is not None:
            return int(parsed.port)
    except Exception:  # noqa: BLE001
        pass

    if ":" in raw:
        tail = raw.rsplit(":", 1)[-1]
        if tail.isdigit():
            return int(tail)

    raise ValueError(f"Invalid AUTHZ_PORT value: {raw}")


def _read_port() -> int:
    raw = os.getenv("AUTHZ_BIND_PORT") or os.getenv("AUTHZ_PORT", "8081")
    return _parse_port(raw)


app = FastAPI(title="AuthZ", version="1.0.0")


@app.exception_handler(OperationalError)
def handle_db_operational_error(_: Request, __: OperationalError) -> JSONResponse:
    return JSONResponse(
        status_code=503,
        content={"detail": "Database temporarily unavailable. Retry in a few seconds."},
    )


@app.get("/health")
def health() -> Dict[str, str]:
    return {"status": "ok"}


class LoginRequest(BaseModel):
    username: Optional[str] = None
    email: Optional[str] = None
    password: str


class RegisterRequest(BaseModel):
    username: str
    email: str
    password: str
    is_active: bool = True


class LoginResponse(BaseModel):
    access_token: str
    token_type: str = "bearer"
    expires_in: int


class MeResponse(BaseModel):
    id: int
    username: str
    email: str
    is_active: bool


@app.post("/login", response_model=LoginResponse)
def login(payload: LoginRequest) -> LoginResponse:
    if not payload.username and not payload.email:
        raise HTTPException(status_code=400, detail="username or email is required")

    try:
        uow = build_uow_from_env()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail="Database not configured") from exc

    with uow:
        user = None
        if payload.username:
            user = uow.repos.users.get_by_username(payload.username)
        if user is None and payload.email:
            user = uow.repos.users.get_by_email(payload.email)

        if not user:
            raise HTTPException(status_code=401, detail="Invalid credentials")
        if REQUIRE_ACTIVE_USER and not getattr(user, "is_active", False):
            raise HTTPException(status_code=403, detail="User inactive")
        if not _verify_password(payload.password, user.password_hash):
            raise HTTPException(status_code=401, detail="Invalid credentials")

        user_id = getattr(user, "id", None)
        if user_id is None:
            raise HTTPException(status_code=500, detail="User id missing")

    token = _encode_jwt(int(user_id))
    return LoginResponse(access_token=token, expires_in=AUTHZ_TOKEN_TTL)


@app.post("/register", response_model=LoginResponse)
def register(payload: RegisterRequest) -> LoginResponse:
    username = payload.username.strip()
    email = payload.email.strip()
    password = payload.password

    if not username:
        raise HTTPException(status_code=400, detail="username is required")
    if not email:
        raise HTTPException(status_code=400, detail="email is required")
    if not password:
        raise HTTPException(status_code=400, detail="password is required")

    try:
        uow = build_uow_from_env()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail="Database not configured") from exc

    with uow:
        if uow.repos.users.get_by_username(username):
            raise HTTPException(status_code=409, detail="Username already exists")
        if uow.repos.users.get_by_email(email):
            raise HTTPException(status_code=409, detail="Email already exists")

        created = uow.repos.users.save(
            ModelUser(
                username=username,
                email=email,
                password_hash=_hash_password(password),
                is_active=payload.is_active,
            )
        )
        user_id = getattr(created, "id", None)
        if user_id is None:
            raise HTTPException(status_code=500, detail="User id missing")
        uow.commit()

    token = _encode_jwt(int(user_id))
    return LoginResponse(access_token=token, expires_in=AUTHZ_TOKEN_TTL)


@app.get("/me", response_model=MeResponse)
def me(request: Request) -> MeResponse:
    try:
        token = _extract_bearer(request.headers.get("authorization"))
        payload = _decode_jwt(token)
    except InvalidTokenError as exc:
        raise HTTPException(status_code=401, detail="Invalid token") from exc
    except RuntimeError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc

    user_id_raw = _get_claim(payload, JWT_USER_CLAIM)
    if user_id_raw is None:
        raise HTTPException(status_code=401, detail="Missing user claim")
    try:
        user_id = int(user_id_raw)
    except (TypeError, ValueError) as exc:
        raise HTTPException(status_code=401, detail="Invalid user claim") from exc

    try:
        uow = build_uow_from_env()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail="Database not configured") from exc

    with uow:
        user = uow.repos.users.get(user_id)
        if not user:
            raise HTTPException(status_code=404, detail="User not found")
        if REQUIRE_ACTIVE_USER and not getattr(user, "is_active", False):
            raise HTTPException(status_code=403, detail="User inactive")

        return MeResponse(
            id=int(getattr(user, "id")),
            username=str(getattr(user, "username")),
            email=str(getattr(user, "email")),
            is_active=bool(getattr(user, "is_active", False)),
        )


@app.api_route("/authorize", methods=["GET", "HEAD"])
def authorize(request: Request, response: Response) -> Dict[str, Any]:
    try:
        token = _extract_bearer(request.headers.get("authorization"))
        payload = _decode_jwt(token)
    except InvalidTokenError as exc:
        raise HTTPException(status_code=401, detail="Invalid token") from exc
    except RuntimeError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc

    user_id_raw = _get_claim(payload, JWT_USER_CLAIM)
    if user_id_raw is None:
        raise HTTPException(status_code=401, detail="Missing user claim")
    try:
        user_id = int(user_id_raw)
    except (TypeError, ValueError) as exc:
        raise HTTPException(status_code=401, detail="Invalid user claim") from exc

    try:
        uow = build_uow_from_env()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail="Database not configured") from exc

    with uow:
        user = uow.repos.users.get(user_id)
        if not user:
            raise HTTPException(status_code=403, detail="User not found")
        if REQUIRE_ACTIVE_USER and not getattr(user, "is_active", False):
            raise HTTPException(status_code=403, detail="User inactive")

        sim_id = _extract_sim_id(request)
        if sim_id is None:
            if _allows_user_only_access(request):
                response.headers["X-User-Id"] = str(user_id)
                return {"ok": True}
            raise HTTPException(status_code=400, detail="Simulation id not found")

        sim = uow.repos.simulations.get(sim_id)
        if not sim or sim.id_user != user_id:
            raise HTTPException(status_code=403, detail="Forbidden")

    response.headers["X-User-Id"] = str(user_id)
    response.headers["X-Simulation-Id"] = str(sim_id)
    return {"ok": True}


if __name__ == "__main__":
    import uvicorn

    host = os.getenv("AUTHZ_HOST", "0.0.0.0")
    port = _read_port()
    uvicorn.run(app, host=host, port=port)
