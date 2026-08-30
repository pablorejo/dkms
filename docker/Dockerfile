# Imagen de un módulo dkms_rust (qkc | orr | dkms | sdn | quditto).
# Build (desde la raíz del repo):
#   docker build -f docker/Dockerfile --build-arg MODULE=qkc -t ORG/qkc .
# Multi-arch: ver docker/docker-bake.hcl.
# syntax=docker/dockerfile:1

ARG MODULE=qkc

# ── build ────────────────────────────────────────────────────────────────
# bookworm trae gcc-12, que aws-lc-sys (dep de rustls) exige (gcc-10 lo rechaza).
# Compila los 5 binarios de una pasada y SIN usar ARG MODULE: así la etapa es
# idéntica para los 5 targets del bake y buildx la construye una sola vez.
FROM rust:1.88-bookworm AS build
# cmake + clang/libclang: highs-sys (backend del LP del SDN) compila HiGHS con
# cmake y genera sus bindings con bindgen, que carga libclang en tiempo de build.
RUN apt-get update && apt-get install -y --no-install-recommends \
      protobuf-compiler cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --bin qkc --bin orr --bin dkms --bin sdn --bin quditto

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
