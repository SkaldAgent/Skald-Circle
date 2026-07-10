# Honcho — self-hosted Docker package

This folder contains a ready-to-run Docker Compose setup for [Honcho](https://honcho.dev),
the memory server used by the personal-agent's Honcho plugin
([`src/plugin/honcho/`](../src/plugin/honcho/)).

---

## What runs

| Service | Image | Port | Role |
| --- | --- | --- | --- |
| `api` | `ghcr.io/plastic-labs/honcho:latest` | **8000** | REST API (the endpoint personal-agent talks to) |
| `deriver` | same image | — | Background worker: extracts conclusions, summaries, peer representations |
| `db` | `pgvector/pgvector:pg17` | 5432 (internal) | PostgreSQL + pgvector (vector search) |
| `redis` | `redis:7-alpine` | 6379 (internal) | Cache for session context |

Data is stored in named Docker volumes (`honcho_db`, `honcho_redis`) and survives container restarts.

---

## Prerequisites

- **Docker** ≥ 24 with **Compose** plugin (`docker compose version`)
- An LLM API key (OpenAI by default; OpenRouter and Ollama also supported — see [LLM providers](#llm-providers))

---

## Quick start

```sh
# 1. Copy the env template
cp .env.example .env

# 2. Set your LLM key (minimum required)
#    Open .env and fill in LLM_OPENAI_API_KEY=sk-...

# 3. Start all services (detached)
docker compose up -d

# 4. Verify the API is up
curl http://localhost:8000/health
# → {"status":"ok"}

# 5. Open interactive API docs
open http://localhost:8000/docs
```

The first startup takes ~30 s while Docker pulls the images and the database runs migrations.

---

## Connect personal-agent

Enable the Honcho plugin in personal-agent by asking the main agent or using the REST API:

```http
PUT /api/plugins/honcho
Content-Type: application/json

{
  "enabled": true,
  "config": {
    "base_url":     "http://localhost:8000",
    "api_key":      "",
    "workspace_id": "personal-agent"
  }
}
```

- `api_key` — leave empty when `HONCHO_AUTH_TOKEN` is not set in `.env`.
- `workspace_id` — any string; used to namespace workspaces inside Honcho.

---

## LLM providers

Honcho needs an LLM to run the **deriver** (background memory extraction). The API itself
works without it, but no long-term conclusions will be built.

### OpenAI (default)

```dotenv
LLM_OPENAI_API_KEY=sk-...
```

Defaults to `gpt-4o-mini` for text generation and `text-embedding-3-small` for embeddings.

### OpenRouter

```dotenv
LLM_OPENAI_API_KEY=sk-or-...
DERIVER_MODEL_CONFIG__TRANSPORT=openai
DERIVER_MODEL_CONFIG__MODEL=openai/gpt-4o-mini
DERIVER_MODEL_CONFIG__OVERRIDES__BASE_URL=https://openrouter.ai/api/v1
```

Gives access to many models (Anthropic, Mistral, Gemini, …) on a single key.

### Ollama (fully local — no data leaves the machine)

Requires [Ollama](https://ollama.com) running on the host with a function-calling model
and an embedding model:

```sh
ollama pull llama3.3:70b
ollama pull nomic-embed-text
```

```dotenv
LLM_OPENAI_API_KEY=ollama
DERIVER_MODEL_CONFIG__TRANSPORT=openai
DERIVER_MODEL_CONFIG__MODEL=llama3.3:70b
DERIVER_MODEL_CONFIG__OVERRIDES__BASE_URL=http://host.docker.internal:11434/v1
LLM_EMBEDDING_API_KEY=ollama
LLM_EMBEDDING_BASE_URL=http://host.docker.internal:11434/v1
LLM_EMBEDDING_MODEL=nomic-embed-text
```

`host.docker.internal` resolves to the host machine from inside the container (works on
macOS and Windows; on Linux add `--add-host=host.docker.internal:host-gateway` to the
compose service if needed).

---

## Useful commands

```sh
# Start / stop
docker compose up -d
docker compose down

# Follow logs
docker compose logs -f api
docker compose logs -f deriver

# Restart only the API after a config change
docker compose restart api

# Stop and wipe all data (destructive!)
docker compose down -v

# Upgrade to a newer Honcho image
docker compose pull
docker compose up -d
```

---

## Build from source (alternative)

If the published image is unavailable or you want to run unreleased code:

```sh
# Clone the official Honcho repository next to this folder
git clone https://github.com/plastic-labs/honcho honcho-src

# In docker-compose.yml, replace the api/deriver `image:` lines with:
#   build:
#     context: ./honcho-src

docker compose up -d --build
```

---

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `api` container exits immediately | DB not ready | Check `docker compose logs db`; wait for "database system is ready" |
| `deriver` keeps restarting | Invalid LLM key or unreachable endpoint | Check `docker compose logs deriver`; verify `.env` |
| `curl http://localhost:8000/health` returns connection refused | Wrong port or container not started | Run `docker compose ps`; check `HONCHO_PORT` in `.env` |
| personal-agent logs `honcho: session_context failed` | API unreachable | Verify `base_url` in plugin config; check firewall / VPN |
| Conclusions not appearing after several chats | Deriver not running | Run `docker compose logs deriver`; check LLM key |

---

## References

- [Honcho GitHub](https://github.com/plastic-labs/honcho)
- [Honcho docs](https://docs.honcho.dev)
- [Self-hosting guide (official)](https://docs.honcho.dev/v3/contributing/self-hosting)
- [personal-agent Honcho plugin docs](../docs/honcho.md)
- [personal-agent Memory architecture](../docs/memory.md)
