# syntax=docker/dockerfile:1
# Image for the gPTP lab: the gptp-gm binary plus iproute2 (ip, tc/netem).
# Build context is the repository root (see lab/compose.yaml).

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/gptp-gm /gptp-gm

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /gptp-gm /usr/local/bin/gptp-gm
COPY lab/switch.sh /usr/local/bin/lab-switch
