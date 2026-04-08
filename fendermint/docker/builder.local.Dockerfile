# syntax=docker/dockerfile:1

# Builder — uses fendermint-deps as base for pre-compiled dependencies.
# fendermint-deps is auto-built by the Makefile if it doesn't exist.
FROM fendermint-deps:latest AS builder

WORKDIR /app

COPY . .

RUN rustup install 1.81.0 && \
  rustup target add aarch64-unknown-linux-gnu --toolchain 1.81.0 && \
  rustup component add --toolchain 1.81.0-aarch64-unknown-linux-gnu rustfmt && \
  RUST_LOG=trace cargo install --locked --root output --path fendermint/app && \
  RUST_LOG=trace cargo install --locked --root output --path ipc/cli
