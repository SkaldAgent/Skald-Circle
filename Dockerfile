# syntax=docker/dockerfile:1
# Includes the full Rust toolchain + source so the app can rebuild itself
# via run.sh (exit -1 → cargo build → restart).
#
# Multi-platform build (amd64 + arm64):
#   docker buildx build --platform linux/amd64,linux/arm64 -t personal-agent:latest .
#
# Single-platform local run:
#   docker build -t personal-agent .
#   docker run -p 3000:3000 \
#     -v /var/run/docker.sock:/var/run/docker.sock \
#     -v ./config.yml:/app/config.yml \
#     -v ./data:/app/data \
#     -v ./database.db:/app/database.db \
#     personal-agent

FROM rust:1-bookworm

# Docker CLI — lets the agent spawn MCP servers via `docker run` using the
# host daemon. Mount the socket at runtime:
#   -v /var/run/docker.sock:/var/run/docker.sock
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg \
        | gpg --dearmor -o /etc/apt/keyrings/docker.gpg \
    && echo \
        "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
        https://download.docker.com/linux/debian bookworm stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update && apt-get install -y --no-install-recommends \
        docker-ce-cli \
        clang \
        libclang-dev \
        cmake \
        python3 \
        python3-pip \
        python3-venv \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# whisper-local is excluded (requires native audio hardware, not available in Docker)

# Pre-fetch and compile dependencies before copying the full source.
# This layer is cached as long as Cargo.toml / Cargo.lock don't change.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release --no-default-features && \
    rm -rf src

# Copy source and build the real binary.
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    touch src/main.rs && \
    cargo build --release --no-default-features && \
    cp target/release/skald /app/skald

# Runtime assets and supervisor.
COPY web              ./web
COPY agents           ./agents
COPY skills           ./skills
COPY default.config.yaml ./
COPY requirements.txt ./
COPY run-docker.sh    ./run-docker.sh
RUN chmod +x run-docker.sh

# Bake a Docker-appropriate default config: bind on all interfaces so the app
# is reachable from outside the container. Port stays 3000 internally;
# map it at runtime with -p 3001:3000 (or any host port you prefer).
# Users can override at runtime with: -v ./config.yml:/app/config.yml
RUN sed 's/host: 127.0.0.1/host: 0.0.0.0/' default.config.yaml > config.yml

# Mount these at runtime — never bake secrets or state into the image:
#   -v ./config.yml:/app/config.yml       (server config — overrides the baked default)
#   -v ./data:/app/data                   (memory files)
#   -v ./database.db:/app/database.db     (chat history)
VOLUME ["/app/data"]

EXPOSE 3000

ENV RUST_LOG=skald=debug,info

# run.sh is the supervisor: on exit -1 it runs `cargo run` to rebuild+restart.
# On a plain restart (no source change) `cargo run` is near-instant.
ENTRYPOINT ["./run-docker.sh"]
