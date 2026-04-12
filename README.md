# copilot-api-proxy

A reverse proxy for GitHub Copilot that exposes OpenAI-compatible `/v1/*` routes, an Anthropic-compatible `/v1/messages` surface with native Claude model passthrough, Amp provider and management routes, and Droid LLM routes that can be backed by Copilot while forwarding Factory control-plane traffic upstream by default. OpenAI requests are forwarded mostly unchanged; native Claude models on Anthropic routes are forwarded directly while non-Claude Anthropic and Gemini compatibility routes translate to Copilot's OpenAI-style upstream. Optional `--amp-local` and `--droid-local` modes serve supported management APIs locally with strict fallback blocking for unsupported upstream routes.

> [!WARNING]
> This is a reverse-engineered proxy of GitHub Copilot API. It is not supported by GitHub, and may break unexpectedly. Use at your own risk.

> [!WARNING]
> **GitHub Security Notice:**
> Excessive automated or scripted use of Copilot (including rapid or bulk requests, such as via automated tools) may trigger GitHub's abuse-detection systems.
> You may receive a warning from GitHub Security, and further anomalous activity could result in temporary suspension of your Copilot access.
>
> GitHub prohibits use of their servers for excessive automated bulk activity or any activity that places undue burden on their infrastructure.
>
> Please review:
>
> - [GitHub Acceptable Use Policies](https://docs.github.com/site-policy/acceptable-use-policies/github-acceptable-use-policies#4-spam-and-inauthentic-activity-on-github)
> - [GitHub Copilot Terms](https://docs.github.com/site-policy/github-terms/github-terms-for-additional-products-and-features#github-copilot)
>
> Use this proxy responsibly to avoid account restrictions.

## Features

- OpenAI-compatible passthrough on `/v1/*`
- Anthropic-compatible `/v1/messages` and `/v1/messages/count_tokens` with native Claude model passthrough
- Amp provider routes for OpenAI, Anthropic, and Gemini clients
- Droid LLM routes for OpenAI Responses, Anthropic Messages, and Gemini generateContent-compatible requests
- Amp management proxy by default for `/api/*` and RSS routes, plus browser redirects for `/threads*`, `/auth*`, `/docs*`, and `/settings*`
- Factory control-plane proxy by default for Droid `/api/cli/*`, `/api/organization/*`, `/api/sessions/*`, and telemetry routes
- Optional `--amp-local` mode for local `/api/threads/*`, `/api/internal`, telemetry, labels, and user info endpoints, with strict fallback blocking for unsupported Amp routes
- Optional `--droid-local` mode for local Droid control-plane stubs, session listing, session writes, and telemetry endpoints, with strict fallback blocking for unsupported Droid routes
- Pluggable web search backends for Amp local mode: `jina`, `tavily`, `brave`, `searxng`, `model`, or `none`
- Streaming, tool/function calling, and vision support
- Sticky `X-Initiator` inference for multi-turn requests
- GitHub OAuth device flow authentication
- Background Copilot token refresh
- User-level service install via `service-manager`

## Requirements

- A GitHub account with an active Copilot subscription

## Installation

### From source

```bash
cargo build --release
```

The binary will be at `target/release/copilot-api-proxy`.

## Quick Start

### 1. Authenticate once

```bash
copilot-api-proxy auth
```

The command prints the GitHub device-flow URL and user code in the terminal, then stores the GitHub token at `~/.local/share/copilot-api-proxy/github_token`.

### 2. Start the proxy

```bash
# Default port: 9876
copilot-api-proxy server

# Custom port
copilot-api-proxy server --port 8080

# With debug logging
copilot-api-proxy server --log-level debug

# Local Amp API subset from local thread data
copilot-api-proxy server --amp-local

# Local Amp mode with an explicit search backend
copilot-api-proxy server --amp-local --search-provider jina

# Local Amp mode using Copilot Responses API web search
copilot-api-proxy server --amp-local --search-provider model --search-model gpt-5-mini

# Local Droid control-plane subset
copilot-api-proxy server --droid-local

# Both local compatibility layers
copilot-api-proxy server --amp-local --droid-local
```

### 3. Point clients at the proxy

Use `http://localhost:9876` as the base URL for OpenAI-compatible and Anthropic-compatible clients. Amp clients can use the same server for provider and management routes.

For Droid:

```bash
FACTORY_API_BASE_URL=http://localhost:9876 \
FACTORY_APP_BASE_URL=http://localhost:9876 \
droid exec "say hello"
```

By default, this proxy handles Droid LLM calls locally through Copilot and forwards Droid control-plane routes to Factory. Add `--droid-local` if you want a strict local control-plane subset instead.

## API Surfaces

| Route | Method | Behavior |
|-------|--------|----------|
| `/v1/{*path}` | Any | Forwards to `https://api.individual.githubcopilot.com/{*path}` after stripping `/v1/`. `/chat/completions` and `/responses` get initiator and vision inference. |
| `/v1/messages` | POST | Native Claude models are forwarded directly to Copilot `/v1/messages`; other models are converted to OpenAI chat/completions and back. |
| `/v1/messages/count_tokens` | POST | Native Claude models are forwarded to Copilot `/v1/messages/count_tokens`; other models use local `tiktoken-rs` estimation. |
| `/api/provider/openai/{version}/{*path}` | Any | Amp OpenAI provider routes proxied through Copilot. |
| `/api/provider/anthropic/{version}/messages` | POST | Native Claude models forwarded via Copilot `/v1/messages` (except lightweight user-initiated haiku which is rewritten to `gpt-5-mini`); other models converted through Copilot `/chat/completions`. |
| `/api/provider/anthropic/{version}/messages/count_tokens` | POST | Native Claude models forwarded to Copilot `/v1/messages/count_tokens`; other models use local token estimation. |
| `/api/provider/google/{version}/models/{model}:{action}` | POST | Gemini `generateContent`, `streamGenerateContent`, and `countTokens` translated through Copilot. |
| `/api/llm/o/v1/{*path}` | Any | Droid OpenAI-like LLM routes handled locally through Copilot. `droid` currently uses `/api/llm/o/v1/responses` for GPT-family models. |
| `/api/llm/a/v1/messages` and `/api/llm/a/v1/messages/count_tokens` | POST | Droid Anthropic-like LLM routes handled locally through Copilot. |
| `/api/llm/g/v1/generate` | POST | Droid Gemini LLM route handled locally by translating Gemini-native requests through Copilot. |
| `/api/cli/*`, `/api/organization/*`, `/api/sessions/*`, `/api/telemetry/*` | Varies | Droid control-plane routes proxied to `https://api.factory.ai` or `FACTORY_UPSTREAM_URL` by default. Under `--droid-local`, the supported local subset is served directly and unsupported routes return `501 Not Implemented`. |
| `/api/threads/find`, `/api/threads/{id}.md`, `/api/internal`, `/api/telemetry`, `/api/durable-thread-workers/*`, `/api/users/*`, `/api/attachments` | Varies | Handled locally only when `--amp-local` is enabled. |
| `/news.rss` | Any | Proxied to Amp upstream by default. Served as a small local RSS stub when `--amp-local` is enabled. |
| Other unsupported Amp management routes such as `/api/*` fallbacks and `/threads.rss` | Any | Proxied to `https://ampcode.com` or `AMP_UPSTREAM_URL` by default. Under `--amp-local`, these routes return `501 Not Implemented` instead of proxying upstream. |
| Browser-facing root routes such as `/auth*`, `/threads*`, `/docs*`, and `/settings*` | Any | Redirected to `https://ampcode.com` or `AMP_UPSTREAM_URL` so browser cookies stay on the Amp domain, including under `--amp-local`. |

## Usage Examples

### OpenAI Chat Completions

```bash
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini-2024-07-18",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### OpenAI Responses API

```bash
curl -X POST http://localhost:9876/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-5", "input": "Hello"}'
```

### Anthropic Messages API

```bash
curl -X POST http://localhost:9876/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### Anthropic Token Counting

```bash
curl -X POST http://localhost:9876/v1/messages/count_tokens \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### Gemini Provider Route

```bash
curl -X POST \
  "http://localhost:9876/api/provider/google/v1beta/models/gemini-2.5-pro:generateContent" \
  -H "Content-Type: application/json" \
  -d '{
    "contents": [{"role": "user", "parts": [{"text": "Hello"}]}]
  }'
```

### List Models

```bash
curl http://localhost:9876/v1/models
```

## System Service

Install as a user-level service:

```bash
# Install the service (default port: 9876)
copilot-api-proxy service install

# Install with custom port
copilot-api-proxy service install --port 8080

# Uninstall the service
copilot-api-proxy service uninstall
```

## Configuration

### Token Storage

- Token path: `~/.local/share/copilot-api-proxy/github_token`
- Directory permissions: `0700`
- File permissions: `0600`
- Load order: `GITHUB_TOKEN` env var, then the token file

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `GITHUB_TOKEN` | Override the stored GitHub token | Token file |
| `ANTHROPIC_API_KEY` | Require an API key for the direct `/v1/messages` route | Unset |
| `BIG_MODEL` | Upstream model used for Anthropic `opus` requests | `claude-opus-4.5` |
| `MIDDLE_MODEL` | Upstream model used for Anthropic `sonnet` requests | `claude-sonnet-4.5` |
| `SMALL_MODEL` | Upstream model used for Anthropic `haiku` requests | `claude-haiku-4.5` |
| `MAX_TOKENS_LIMIT` | Maximum Anthropic `max_tokens` forwarded upstream | `4096` |
| `MIN_TOKENS_LIMIT` | Minimum Anthropic `max_tokens` forwarded upstream | `100` |
| `AMP_API_KEY` | API key injected when proxying Amp management routes | Amp secrets file or unset |
| `AMP_UPSTREAM_URL` | Base URL for Amp management routes when proxying is allowed | `https://ampcode.com` |
| `AMP_THREADS_DIR` | Override the local Amp thread directory used by `--amp-local` | `~/.local/share/amp/threads` |
| `FACTORY_API_BASE_URL` | Base URL that Droid uses for Factory API calls when you point Droid at this proxy | Unset |
| `FACTORY_APP_BASE_URL` | Base URL that Droid uses for Factory app/web routes when you point Droid at this proxy | Unset |
| `FACTORY_TELEMETRY_INGEST_URL` | Base URL that Droid uses for telemetry ingest when you want telemetry to hit this proxy or another override | Unset |
| `FACTORY_WORKOS_BASE_URL` | Base URL that Droid uses for WorkOS-related requests when overriding Factory endpoints | Unset |
| `FACTORY_UPSTREAM_URL` | Base URL for Droid control-plane routes when proxying is allowed | `https://api.factory.ai` |
| `DROID_LOCAL_USER_ID` | User ID returned by `--droid-local` `whoami` | `u_local` |
| `DROID_LOCAL_ORG_ID` | Org ID returned by `--droid-local` `whoami` and feature flags | `o_local` |
| `JINA_API_KEY` | Optional API key for Jina search/reader backend | Unset |
| `TAVILY_API_KEY` | API key for `--search-provider tavily` | Unset |
| `BRAVE_API_KEY` | API key for `--search-provider brave` | Unset |
| `SEARXNG_URL` | Base URL for `--search-provider searxng` | Unset |
| `RUST_LOG` | Overrides the logger filter entirely | Unset |

### Logging

If `RUST_LOG` is unset, `copilot-api-proxy server --log-level <level>` builds the filter `copilot_api_proxy=<level>,tower_http=<level>`.

```bash
# Debug logging for the proxy and HTTP layer
copilot-api-proxy server --log-level debug

# Explicit env-based filter
RUST_LOG=copilot_api_proxy=debug,tower_http=debug copilot-api-proxy server
```

### Anthropic Route Authentication

To require an API key for the direct `/v1/messages` route:

```bash
ANTHROPIC_API_KEY=your-secret-key copilot-api-proxy server
```

Clients must then provide the key via `x-api-key` or `Authorization: Bearer ...`.

### Amp Local Mode

`--amp-local` serves a subset of Amp management APIs from local data instead of forwarding them upstream:

- `/api/threads/find`
- `/api/threads/{id}.md`
- `/api/internal`
- `/api/telemetry`
- `/api/durable-thread-workers/*`
- `/api/users/*`
- `/api/attachments`

In addition:

- `/news.rss` is served by a local stub feed
- any other would-be Amp upstream fallback returns `501 Not Implemented` and logs an `amp_proxy` error instead of proxying upstream

That means unsupported root-level routes such as `/threads*`, `/auth*`, `/docs*`, `/settings*`, and `/threads.rss` no longer silently escape to `ampcode.com` under `--amp-local`.

Available search backends for local mode:

- `jina` uses Jina Search + Reader and optionally `JINA_API_KEY`
- `tavily` requires `TAVILY_API_KEY`
- `brave` requires `BRAVE_API_KEY` and uses Jina Reader for page extraction
- `searxng` requires `SEARXNG_URL` and uses Jina Reader for page extraction
- `model` uses Copilot `/v1/responses` with the `web_search` tool; `--search-model` defaults to `gpt-5-mini`
- `none` disables web search/page extraction

### Droid Local Mode

`--droid-local` serves a strict local subset of Droid control-plane APIs instead of forwarding them to Factory:

- `GET /api/cli/whoami`
- `GET /api/sessions`
- `GET /api/organization/managed-settings`
- `GET /api/feature-flags`
- `POST /api/sessions/create`
- `POST /api/sessions/{id}/update-settings`
- `POST /api/sessions/{id}/message/create`
- `POST /api/sessions/{id}/update-title`
- `POST /api/sessions/{id}/droid-status`
- `POST /api/telemetry/cli-ingest`
- `POST /api/telemetry/otlp/traces/ingest`
- `POST /api/llm/custom/usage`
- `POST /api/llm/failed-requests`

In both modes, Droid LLM routes stay local:

- `/api/llm/o/v1/*`
- `/api/llm/a/v1/*`
- `/api/llm/g/v1/generate`

Under `--droid-local`, unsupported non-LLM Droid routes return `501 Not Implemented` instead of proxying upstream.

## How It Works

1. `auth` runs GitHub's OAuth device flow and stores the GitHub token locally.
2. The server exchanges that GitHub token for a Copilot API token.
3. `TokenManager` refreshes the Copilot token in the background before expiry.
4. OpenAI-compatible `/v1/*` requests are forwarded to `api.individual.githubcopilot.com` with Copilot headers injected.
5. Native Claude models on Anthropic routes are forwarded directly to Copilot's `/v1/messages` endpoint; non-Claude Anthropic models and Gemini compatibility routes translate request and response formats around the same Copilot upstream.
6. Amp provider routes are handled locally when supported; Amp management routes are proxied to `ampcode.com` by default, while `--amp-local` serves the supported local subset, stubs `/news.rss`, and rejects other Amp fallbacks with `501 Not Implemented`.
7. Droid LLM routes are always handled locally through Copilot-compatible adapters. Droid control-plane routes are proxied to Factory by default, while `--droid-local` serves the supported local subset and rejects unsupported upstream fallbacks with `501 Not Implemented`.

### Sticky Inference

For OpenAI chat/responses requests and converted Anthropic/Gemini requests, the proxy inspects message history:

- Requests with only user turns are marked `X-Initiator: user`
- Requests containing prior `assistant` or `tool` turns are marked `X-Initiator: agent`
- Vision inputs set `Copilot-Vision-Request: true`

## License

MIT
