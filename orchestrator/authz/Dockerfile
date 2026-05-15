# syntax=docker/dockerfile:1.7
#
# AuthZ image. Shares the orchestrator's models/persistence/requirements.txt
# so build context is `orchestrator/`, not the crate-level repo root.
#
# Build:  docker build -f orchestrator/authz/Dockerfile -t pablopio/authz:vX orchestrator
FROM python:3.12-slim-bookworm AS runtime

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PYTHONPATH=/app \
    AUTHZ_HOST=0.0.0.0 \
    AUTHZ_PORT=8081

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        libpq-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# Bring the shared orchestrator code so authz can import models, persistence.
COPY models ./models
COPY persistence ./persistence
COPY authz ./authz

EXPOSE 8081

CMD ["python", "-m", "authz.main"]
