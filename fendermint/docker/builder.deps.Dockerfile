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

# The ipc_actors_abis crate needs compiled Solidity contracts (contracts/out/)
# which we don't have here. Remove its source so cargo skips it and everything
# that depends on it. All other crates compile with real source.
# Crates depending on ipc_actors_abis: ipc-api, ipc-provider,
# fendermint_vm_actor_interface, fendermint_vm_topdown, fendermint_vm_interpreter,
# fendermint_testing_contract-test, fendermint_testing_materializer.
RUN rm -rf contracts/binding/src && mkdir -p contracts/binding/src && \
    echo '' > contracts/binding/src/lib.rs

# Also remove build.rs files for actor bundling (needs wasm compilation setup)
RUN rm -f fendermint/actors/build.rs

# Pre-compile everything possible. Crates depending on ipc_actors_abis will fail,
# but all external deps + most internal crates get cached.
RUN cargo build --locked --release -p fendermint_app -p ipc-cli 2>/dev/null || true

# Clean artifacts ONLY for crates that failed or used dummy source,
# so they get properly rebuilt with real source in builder.local.Dockerfile.
RUN rm -rf target/release/.fingerprint/ipc_actors_abis* \
    target/release/.fingerprint/ipc-api* \
    target/release/.fingerprint/ipc_api* \
    target/release/.fingerprint/ipc-provider* \
    target/release/.fingerprint/ipc_provider* \
    target/release/.fingerprint/fendermint_vm_actor_interface* \
    target/release/.fingerprint/fendermint_vm_topdown* \
    target/release/.fingerprint/fendermint_vm_interpreter* \
    target/release/.fingerprint/fendermint_testing* \
    target/release/.fingerprint/fendermint_actors* \
    target/release/.fingerprint/fendermint_app* \
    target/release/.fingerprint/ipc-cli* \
    target/release/.fingerprint/ipc_cli* \
    target/release/deps/libipc_actors_abis* \
    target/release/deps/libipc_api* \
    target/release/deps/libipc_provider* \
    target/release/deps/libfendermint_vm_actor_interface* \
    target/release/deps/libfendermint_vm_topdown* \
    target/release/deps/libfendermint_vm_interpreter* \
    target/release/deps/libfendermint_testing* \
    target/release/deps/libfendermint_actors* \
    target/release/deps/libfendermint_app* \
    target/release/deps/libipc_cli* \
    target/release/build/ipc-api* \
    target/release/build/ipc_actors_abis* \
    target/release/build/fendermint_actors* \
    target/release/incremental/ \
    2>/dev/null || true
