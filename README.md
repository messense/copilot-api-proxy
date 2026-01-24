# copilot-api-proxy

A reverse proxy for the GitHub Copilot API that exposes OpenAI and Anthropic compatible endpoints. It forwards requests unchanged and injects the required Copilot authentication headers.

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

- **OpenAI-compatible endpoints** — `/v1/chat/completions`, `/v1/responses`, `/v1/models`
- **Anthropic-compatible endpoint** — `/v1/messages` with full request/response conversion
- **Pure passthrough** — No schema translation for OpenAI endpoints (minimal initiator inference only)
- **Streaming support** — Server-Sent Events (SSE) for both OpenAI and Anthropic formats
- **Tool/function calling** — Full support for tool use in both API formats
- **Vision support** — Image inputs are forwarded with proper Copilot headers
- **GitHub OAuth device flow** — One-time browser-based authentication
- **Automatic token refresh** — Copilot tokens are refreshed in the background
- **System service** — Install as a daemon on macOS/Linux

## Requirements

- A GitHub account with an active Copilot subscription

## Installation

### From source

```bash
cargo build --release
```

The binary will be at `target/release/copilot-api-proxy`.

## Quick Start

### 1. Authenticate (one-time)

```bash
copilot-api-proxy auth
```

This opens a browser for GitHub OAuth and stores the token at `~/.local/share/copilot-api-proxy/github_token`.

### 2. Start the proxy

```bash
# Default port: 9876
copilot-api-proxy server

# Custom port
copilot-api-proxy server --port 8080

# With debug logging
copilot-api-proxy server --log-level debug
```

### 3. Use the API

Point your OpenAI or Anthropic client to `http://localhost:9876`.

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/chat/completions` | POST | OpenAI chat completions |
| `/v1/responses` | POST | OpenAI responses API (gpt-5 only) |
| `/v1/models` | GET | List available models |
| `/v1/messages` | POST | Anthropic messages API (converted to OpenAI) |
| `/health` | GET | Health check |

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

### Streaming Response

```bash
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini-2024-07-18",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
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

### List Models

```bash
curl http://localhost:9876/v1/models
```

## System Service

Install as a system daemon to run automatically on boot:

```bash
# Install the service (default port: 9876)
copilot-api-proxy service install

# Install with custom port
copilot-api-proxy service install --port 8080

# Uninstall the service
copilot-api-proxy service uninstall
```

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `GITHUB_TOKEN` | Override the stored GitHub token | Token file |
| `ANTHROPIC_API_KEY` | Require API key for `/v1/messages` | None (no auth) |
| `BIG_MODEL` | Model for Claude opus requests | `claude-opus-4.5` |
| `MIDDLE_MODEL` | Model for Claude sonnet requests | `claude-sonnet-4.5` |
| `SMALL_MODEL` | Model for Claude haiku requests | `claude-haiku-4.5` |
| `MAX_TOKENS_LIMIT` | Maximum `max_tokens` for Claude requests | `16384` |
| `MIN_TOKENS_LIMIT` | Minimum `max_tokens` for Claude requests | `100` |
| `RUST_LOG` | Logging verbosity | `info` |

### Logging

```bash
# Debug logging for the proxy and HTTP layer
RUST_LOG=copilot_api_proxy=debug,tower_http=debug copilot-api-proxy server

# Trace logging for maximum verbosity
RUST_LOG=trace copilot-api-proxy server
```

### API Key Authentication

To require authentication for the Anthropic-compatible endpoint:

```bash
ANTHROPIC_API_KEY=your-secret-key copilot-api-proxy server
```

Clients must then provide the key via `x-api-key` header or `Authorization: Bearer` header.

## How It Works

1. **Authentication**: Uses GitHub's OAuth device flow to obtain a user token
2. **Token Exchange**: Exchanges the GitHub token for a Copilot API token
3. **Auto-refresh**: Background task refreshes the Copilot token before expiry
4. **Proxying**: Forwards requests to `api.individual.githubcopilot.com` with proper headers
5. **Conversion**: For `/v1/messages`, converts between Anthropic and OpenAI formats

### X-Initiator Header

The proxy infers whether a request is user-initiated or agent-initiated based on the message history:
- **User-initiated**: Consumes premium request quota
- **Agent-initiated**: Uses standard quota

## License

MIT
