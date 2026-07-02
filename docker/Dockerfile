# Imagen de un módulo dkms_rust (qkc | orr | dkms | sdn).
# Build (desde la raíz del repo):
#   docker build -f docker/Dockerfile --build-arg MODULE=qkc -t ORG/qkc .
# Multi-arch: ver docker/docker-bake.hcl.
# syntax=docker/dockerfile:1

ARG MODULE=qkc

# ── build ────────────────────────────────────────────────────────────────
# bookworm trae gcc-12, que aws-lc-sys (dep de rustls) exige (gcc-10 lo rechaza).
FROM rust:1.88-bookworm AS build
ARG MODULE
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p "${MODULE}"

# ── runtime ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
ARG MODULE
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates python3 python3-yaml wget \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/${MODULE} /usr/local/bin/${MODULE}
COPY docker/render_config.py /opt/render_config.py
COPY docker/entrypoint.sh /opt/entrypoint.sh
RUN chmod +x /opt/entrypoint.sh
# El entrypoint sabe qué módulo es por esta variable.
ENV ROLE=${MODULE}
ENTRYPOINT ["/opt/entrypoint.sh"]
