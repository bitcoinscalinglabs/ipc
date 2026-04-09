# syntax=docker/dockerfile:1

# Builder — uses fendermint-deps as base for pre-compiled dependencies.
# fendermint-deps is auto-built by the Makefile if it doesn't exist.
FROM fendermint-deps:latest AS builder

WORKDIR /app

COPY . .

# Same cargo invocation as builder.deps.Dockerfile so fingerprints match
# and cached artifacts from fendermint-deps are actually reused.
RUN cargo build --locked --release -p fendermint_app -p ipc-cli && \
    mkdir -p output/bin && \
    cp target/release/fendermint target/release/ipc-cli output/bin/
