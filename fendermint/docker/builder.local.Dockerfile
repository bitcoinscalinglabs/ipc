# syntax=docker/dockerfile:1

# Pre-compiled-dependencies base image, auto-built by the Makefile.
ARG DEPS_IMAGE=fendermint-deps:latest
FROM ${DEPS_IMAGE} AS builder

WORKDIR /app

COPY . .

# Same cargo invocation as builder.deps.Dockerfile so fingerprints match
# and cached artifacts from fendermint-deps are actually reused.
RUN cargo build --locked --release -p fendermint_app -p ipc-cli && \
    mkdir -p output/bin && \
    cp target/release/fendermint target/release/ipc-cli output/bin/
