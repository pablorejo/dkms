#!/usr/bin/env bash
# Drops the Aurora/Postgres database and recreates it with schema + seed.
#
# Runs all SQL through ephemeral pods inside the cluster because Aurora's
# security group typically only allows the EKS VPC to reach the DB. Local
# psql connections from a developer laptop will be refused.
#
# Reads DB_URL from .env (or env). Connects as the URL's user to "postgres"
# database to issue DROP DATABASE / CREATE DATABASE on the target one.
#
# Usage: scripts/db-reset.sh [--no-seed]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

SEED=1
for arg in "$@"; do
    case "$arg" in
        --no-seed) SEED=0 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${DB_URL:?DB_URL must be set (export it or define in .env)}"
: "${KUBECTL:=kubectl}"
: "${TOOL_NS:=default}"
: "${IMAGE_PREFIX:=pablopio}"
: "${ORCH_IMAGE_TAG:=v2}"

# Parse DB_URL → host/port/user/pass/db.
eval "$(python3 - <<'PYEOF'
import os
from urllib.parse import urlparse, unquote
u = urlparse(os.environ["DB_URL"])
print(f"PGHOST={u.hostname or 'localhost'}")
print(f"PGPORT={u.port or 5432}")
print(f"PGUSER={unquote(u.username or 'postgres')}")
print(f"PGPASSWORD='{unquote(u.password or '')}'")
print(f"PGDATABASE={(u.path or '/postgres').lstrip('/') or 'postgres'}")
PYEOF
)"
export PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE
PGSSLMODE="${PGSSLMODE:-require}"

echo "[db-reset] target: $PGUSER@$PGHOST:$PGPORT/$PGDATABASE (sslmode=$PGSSLMODE)"

# Random suffix so concurrent runs don't collide on the pod name.
RUN_ID="$(date +%s)-$$"

ensure_ns() {
    "$KUBECTL" get namespace "$TOOL_NS" >/dev/null 2>&1 || "$KUBECTL" create namespace "$TOOL_NS"
}

# Run a single SQL string against a specific database via an ephemeral psql pod.
psql_exec() {
    local target_db="$1"
    local sql="$2"
    local pod="psql-cmd-${RUN_ID}-${RANDOM}"
    "$KUBECTL" run "$pod" \
        --namespace="$TOOL_NS" \
        --rm -i --restart=Never \
        --image=postgres:16-alpine \
        --image-pull-policy=IfNotPresent \
        --env="PGHOST=$PGHOST" --env="PGPORT=$PGPORT" \
        --env="PGUSER=$PGUSER" --env="PGPASSWORD=$PGPASSWORD" \
        --env="PGSSLMODE=$PGSSLMODE" \
        --command -- \
        psql -d "$target_db" -v ON_ERROR_STOP=1 -c "$sql"
}

# Stream a local SQL file into the target DB via an ephemeral psql pod.
psql_file() {
    local target_db="$1"
    local file="$2"
    local pod="psql-file-${RUN_ID}-${RANDOM}"
    "$KUBECTL" run "$pod" \
        --namespace="$TOOL_NS" \
        --rm -i --restart=Never \
        --image=postgres:16-alpine \
        --image-pull-policy=IfNotPresent \
        --env="PGHOST=$PGHOST" --env="PGPORT=$PGPORT" \
        --env="PGUSER=$PGUSER" --env="PGPASSWORD=$PGPASSWORD" \
        --env="PGSSLMODE=$PGSSLMODE" \
        --command -- \
        psql -d "$target_db" -v ON_ERROR_STOP=1 -f - \
        < "$file"
}

ensure_ns

echo "[db-reset] terminating active sessions on $PGDATABASE..."
psql_exec postgres "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '$PGDATABASE' AND pid <> pg_backend_pid();" \
    || echo "  (could not terminate sessions; continuing)" >&2

echo "[db-reset] DROP DATABASE \"$PGDATABASE\""
psql_exec postgres "DROP DATABASE IF EXISTS \"$PGDATABASE\";"

echo "[db-reset] CREATE DATABASE \"$PGDATABASE\""
psql_exec postgres "CREATE DATABASE \"$PGDATABASE\";"

# Ensure the dockerhub-pull secret exists in TOOL_NS before we try to run a
# private image. The legacy SQL files in migrations/ are incremental ALTERs
# that assume the base schema already exists; we create that via SQLAlchemy's
# Base.metadata.create_all first.
if [[ -n "${DOCKER_HUB_USERNAME:-}" && -n "${DOCKER_HUB_TOKEN:-}" ]]; then
    "$KUBECTL" -n "$TOOL_NS" create secret docker-registry dockerhub-pull \
        --docker-server="${DOCKER_HUB_SERVER:-https://index.docker.io/v1/}" \
        --docker-username="$DOCKER_HUB_USERNAME" \
        --docker-password="$DOCKER_HUB_TOKEN" \
        --docker-email="${DOCKER_HUB_EMAIL:-}" \
        --dry-run=client -o yaml | "$KUBECTL" apply -f - >/dev/null
fi

# Helper: run a python snippet inside the orchestrator image.
run_in_orchestrator() {
    local pod="orch-tool-${RUN_ID}-${RANDOM}"
    local cmd="$1"
    "$KUBECTL" run "$pod" \
        --namespace="$TOOL_NS" \
        --rm -i --restart=Never \
        --image="${IMAGE_PREFIX}/orchestator:${ORCH_IMAGE_TAG}" \
        --image-pull-policy=Always \
        --env="DB_URL=$DB_URL" \
        --env="PERSISTENCE_BACKEND=sqlalchemy" \
        --overrides='{"spec":{"imagePullSecrets":[{"name":"dockerhub-pull"}]}}' \
        --command -- \
        bash -c "$cmd"
}

echo "[db-reset] creating base schema (SQLAlchemy Base.metadata.create_all)"
# The orchestrator image ships psycopg (v3), not psycopg2. SQLAlchemy
# defaults to psycopg2 for plain "postgresql://" URLs, so we override the
# driver in the URL before passing it to create_engine.
run_in_orchestrator "python -c 'import os; from sqlalchemy import create_engine; from persistence.sqlalchemy.data import Base; url = os.environ[\"DB_URL\"].replace(\"postgresql://\", \"postgresql+psycopg://\", 1); Base.metadata.create_all(create_engine(url)); print(\"base schema created\")'"

MIGRATIONS_DIR="$REPO_ROOT/orchestrator/persistence/sqlalchemy/migrations"
if [[ -d "$MIGRATIONS_DIR" ]]; then
    echo "[db-reset] applying legacy SQL migrations in order"
    for sql in $(ls -1 "$MIGRATIONS_DIR"/*.sql 2>/dev/null | sort); do
        echo "  -> $(basename "$sql")"
        psql_file "$PGDATABASE" "$sql"
    done
fi

echo "[db-reset] alembic stamp head (inside orchestrator image)"
run_in_orchestrator "cd persistence/sqlalchemy && alembic stamp head" \
    || echo "[db-reset] alembic stamp skipped/failed; not fatal" >&2

if [[ "$SEED" -eq 1 ]]; then
    echo "[db-reset] seed_db_from_configs.py (inside orchestrator image)"
    run_in_orchestrator "python seed_db_from_configs.py" \
        || echo "[db-reset] seed FAILED — schema is ready, populate manually" >&2
fi

echo "[db-reset] done."
