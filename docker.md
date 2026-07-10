# Docker — build & run

## Prerequisites

- Docker installed and running

## Build

```sh
docker build -t skald .
```

For multi-platform builds (amd64 + arm64, e.g. to push to a registry):

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t skald:latest .
```

## Run

### Isolated (no persistence)

Everything lives inside the container. Data and chat history are lost when the container is removed.

```sh
docker run -p 3001:3000 skald
```

### Persistent data and database

Mount `./data` (agent memory files) and `./database.db` (chat history, provider config) to keep state across container restarts.

```sh
touch database.db && mkdir -p data
docker run -p 3001:3000 \
  -v ./data:/app/data \
  -v ./database.db:/app/database.db \
  skald
```

### Persistent data only

Keep agent memory files but start with a fresh database each time.

```sh
mkdir -p data
docker run -p 3001:3000 \
  -v ./data:/app/data \
  skald
```

The app will be available at `http://localhost:3001` (the container listens on 3000 internally).

### Custom config

The default `config.yml` is baked into the image (port 3000, host `0.0.0.0`). To override it:

```sh
-v ./config.yml:/app/config.yml
```

### Docker socket (MCP containers)

Add `-v /var/run/docker.sock:/var/run/docker.sock` if you want the agent to be able to start MCP server containers via the host Docker daemon.

### Volume reference

| Volume | Contents |
| --- | --- |
| `./config.yml` | Server configuration — overrides the baked default |
| `./data` | Agent memory files |
| `./database.db` | Chat history and provider/model configuration |
| `/var/run/docker.sock` | Allows the agent to start MCP containers via the host daemon |

## Self-recompilation

The container includes the full Rust toolchain. When the agent calls the `restart` tool, the process exits with code `-1` and the supervisor (`run-docker.sh`) runs `cargo build` inside the container and restarts. Source code changes are only persisted if you also mount `./src`:

```sh
docker run -p 3001:3000 \
  -v ./data:/app/data \
  -v ./database.db:/app/database.db \
  -v ./src:/app/src \
  skald
```

## Running without Docker (local development)

```sh
cargo build
./run.sh
```

`RUST_LOG=skald=debug,info` is set by default; to override locally:

```sh
RUST_LOG=debug ./run.sh
```
