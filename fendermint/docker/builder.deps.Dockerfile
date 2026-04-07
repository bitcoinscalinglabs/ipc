# syntax=docker/dockerfile:1
#
# Pre-compiles Rust dependencies for fendermint and ipc-cli.
# Build once and reuse as the base image for builder.local.Dockerfile.
# Rebuild when Cargo.lock changes.
#
# Build (from ipc repo root):
#   DOCKER_BUILDKIT=1 docker build -f fendermint/docker/builder.deps.Dockerfile -t fendermint-deps:latest .

FROM rust:bookworm

RUN apt-get update && \
    apt-get install -y build-essential clang cmake protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Replace real source with dummies, keep Cargo manifests + lockfile.
# Crates that depend on contract ABI bindings will fail — that's OK,
# we still cache all external dependencies (tokio, libp2p, rocksdb, ethers, etc.).
RUN find . -name "*.rs" -not -name "build.rs" | while read f; do \
      if echo "$f" | grep -q "main.rs"; then echo 'fn main(){}' > "$f"; \
      else echo '' > "$f"; fi; done && \
    find . -name "build.rs" -delete

# Pre-compile all dependencies in release mode (|| true: some internal crates will fail)
RUN cargo build --locked --release -p fendermint_app -p ipc-cli 2>/dev/null || true
