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
RUN cargo chef cook --release --recipe-path recipe.json -p quditto
COPY . .
RUN cargo build --release -p quditto

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/quditto /usr/local/bin/quditto
COPY quditto/config /etc/quditto/config
ENV CONFIG_DIR=/etc/quditto/config
EXPOSE 50055 9104
ENTRYPOINT ["/usr/local/bin/quditto"]
