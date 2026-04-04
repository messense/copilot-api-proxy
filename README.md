# copilot-api-proxy

A reverse proxy for GitHub Copilot that exposes OpenAI-compatible `/v1/*` routes, an Anthropic-compatible `/v1/messages` surface, and Amp provider routes for OpenAI, Anthropic, and Gemini. OpenAI requests are forwarded mostly unchanged; Anthropic and Gemini compatibility routes translate to Copilot's OpenAI-style upstream.

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
- Anthropic-compatible `/v1/messages` and `/v1/messages/count_tokens`
- Amp provider routes for OpenAI, Anthropic, and Gemini clients
- Amp management proxy for `/api/*`, `/threads*`, `/auth*`, `/docs*`, `/settings*`, and RSS routes
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

# Local Amp mode (no ampcode.com dependency)
copilot-api-proxy server --amp-local
```

### 3. Point clients at the proxy

Use `http://localhost:9876` as the base URL for OpenAI-compatible and Anthropic-compatible clients. Amp clients can use the same server for provider and management routes.

## API Surfaces

| Route | Method | Behavior |
|-------|--------|----------|
| `/v1/{*path}` | Any | Forwards to `https://api.individual.githubcopilot.com/{*path}` after stripping `/v1/`. `/chat/completions` and `/responses` get initiator and vision inference. |
| `/v1/messages` | POST | Converts Anthropic Messages API requests to OpenAI chat/completions and converts responses back to Anthropic format. |
| `/v1/messages/count_tokens` | POST | Estimates Anthropic input tokens locally with `tiktoken-rs`. |
| `/api/provider/openai/{version}/{*path}` | Any | Amp OpenAI provider routes proxied through Copilot. |
| `/api/provider/anthropic/{version}/messages` | POST | Amp Anthropic provider route converted through Copilot. |
| `/api/provider/anthropic/{version}/messages/count_tokens` | POST | Local Anthropic token counting for Amp clients. |
| `/api/provider/google/{version}/models/{model}:{action}` | POST | Gemini `generateContent`, `streamGenerateContent`, and `countTokens` translated through Copilot. |
| `/api/*`, `/auth*`, `/threads*`, `/docs*`, `/settings*`, `/news.rss` | Any | Amp management routes proxied to `https://ampcode.com` or `AMP_UPSTREAM_URL`. |

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
| `AMP_UPSTREAM_URL` | Base URL for proxied Amp management routes | `https://ampcode.com` |
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

## How It Works

1. `auth` runs GitHub's OAuth device flow and stores the GitHub token locally.
2. The server exchanges that GitHub token for a Copilot API token.
3. `TokenManager` refreshes the Copilot token in the background before expiry.
4. OpenAI-compatible `/v1/*` requests are forwarded to `api.individual.githubcopilot.com` with Copilot headers injected.
5. Anthropic and Gemini compatibility routes translate request and response formats around the same Copilot upstream.
6. Amp provider routes are handled locally when supported; Amp management routes are proxied to `ampcode.com`.

### Sticky Inference

For OpenAI chat/responses requests and converted Anthropic/Gemini requests, the proxy inspects message history:

- Requests with only user turns are marked `X-Initiator: user`
- Requests containing prior `assistant` or `tool` turns are marked `X-Initiator: agent`
- Vision inputs set `Copilot-Vision-Request: true`

## License

MIT
