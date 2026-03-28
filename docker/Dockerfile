# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        libasound2-dev \
        libopus-dev \
        libssl-dev \
        make \
        pkg-config \
        protobuf-compiler \
        uuid-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libasound2 \
        libopus0 \
        libssl3 \
        libuuid1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /src/target/release/mumble-sip /usr/local/bin/mumble-sip
COPY --from=builder /src/sounds /app/sounds
COPY --from=builder /src/config.toml.example /app/config.toml.example

VOLUME ["/config", "/data"]

ENTRYPOINT ["mumble-sip"]
CMD ["/config/config.toml"]
