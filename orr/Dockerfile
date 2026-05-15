# syntax=docker/dockerfile:1.7
FROM rust:1.88-bookworm AS chef
ENV RUSTUP_TOOLCHAIN=stable
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install --locked cargo-chef@0.1.72

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p orr
COPY . .
RUN cargo build --release -p orr

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --gid 0 --no-create-home orr
COPY --from=builder /app/target/release/orr /usr/local/bin/orr
COPY orr/config /etc/orr/config
ENV CONFIG_DIR=/etc/orr/config
USER 10001
EXPOSE 50052 9101
ENTRYPOINT ["/usr/local/bin/orr"]
