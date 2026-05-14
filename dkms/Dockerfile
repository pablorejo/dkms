FROM rust:1.83-bookworm AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo install --locked cargo-chef
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN cargo install --locked cargo-chef
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p dkms
COPY . .
RUN cargo build --release -p dkms

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/dkms /usr/local/bin/dkms
COPY dkms/config /etc/dkms/config
ENV CONFIG_DIR=/etc/dkms/config
EXPOSE 8080 50054 9103
ENTRYPOINT ["/usr/local/bin/dkms"]
