# CLAUDE.md

## Project Overview

**copilot-api-proxy** is a reverse proxy server written in Rust that provides OpenAI-compatible API endpoints by forwarding requests to the GitHub Copilot API with proper authentication.

**Key Design Philosophy**: This is a **pure reverse proxy** with **auth injection only**. It does NOT translate or modify request/response bodies for OpenAI-compatible endpoints, except for minimal initiator inference on `/v1/chat/completions` and `/v1/responses` to set `X-Initiator`. The Copilot API is already OpenAI-compatible, so the proxy simply injects authentication headers and forwards requests unchanged.

---

## Common Commands

### Build and Run

```bash
# Build the project
cargo build

# Build for release
cargo build --release

# Run the proxy server (default port: 9876)
cargo run -- server

# Run on custom port
cargo run -- server --port 8080

# Run authentication flow (one-time setup)
cargo run -- auth
```

### Testing

```bash
# Run tests
cargo test

# Run tests with output
cargo test -- --nocapture

# Run specific test
cargo test test_name
```

### Manual Testing Endpoints

```bash
# Test chat completions (non-streaming)
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o-mini-2024-07-18", "messages": [{"role": "user", "content": "Hello"}]}'

# Test streaming
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o-mini-2024-07-18", "messages": [{"role": "user", "content": "Hello"}], "stream": true}'

# Test models endpoint
curl http://localhost:9876/v1/models

# Test responses API (gpt-5 only)
curl -X POST http://localhost:9876/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-5", "input": "Hello"}'
```

### Environment Variables

```bash
# GitHub token (overrides file-based token)
export GITHUB_TOKEN=your_github_token

# Logging
export RUST_LOG=copilot_api_proxy=debug,tower_http=debug
```

---

## High-Level Architecture

### Request Flow

```
Client Request (OpenAI format)
    ↓
Axum Router: /v1/{*path} catch-all
    ↓
Generic Proxy Handler (single handler for ALL endpoints)
    ↓
ProxyClient (injects Copilot auth headers)
    ↓
Request analysis (chat/responses only; sets `X-Initiator` and `Copilot-Vision-Request`)
    ↓
TokenManager (background refresh)
    ↓
GitHub Copilot API (api.individual.githubcopilot.com)
    ↓
Response Processing (detect streaming via Content-Type)
    ↓
Client Response (unchanged from upstream)
```

### Key Architectural Decisions

1. **Single Generic Proxy Handler**: One `proxy_handler()` serves ALL `/v1/*` endpoints (chat completions, responses, embeddings, etc.). No endpoint-specific code needed.

2. **No Request/Response Parsing**: Bodies are passed through as `Bytes`. No serde models for API schemas. This ensures forward compatibility with any Copilot API changes. The only exception is minimal parsing of chat/responses bodies to infer `X-Initiator`.

3. **Background Token Refresh**: `TokenManager` spawns a background task that refreshes the Copilot token 60 seconds before expiry. Thread-safe access via `Arc<RwLock>`.

4. **Unified Response Forwarding**: Single `forward_response()` function handles both streaming and non-streaming responses by detecting `Content-Type`.

5. **Path Transformation**: Client requests `/v1/chat/completions` → Upstream receives `/chat/completions` (Copilot API doesn't use version prefixes).

### Module Structure

```
src/
├── main.rs      # CLI entry point (auth/server commands)
├── lib.rs       # Library exports
├── config.rs    # Token path and loading functions
├── auth.rs      # Device flow, token exchange, token manager
├── initiator.rs # Initiator inference for sticky inference
├── proxy.rs     # HTTP client, headers, response forwarding
├── server.rs    # Router, app state, proxy handler
└── error.rs     # Single Error enum with OpenAI responses
```

---

## Critical Implementation Details

### 1. Single Proxy Handler Pattern

**File**: `src/server.rs`

The route `/v1/{*path}` captures everything after `/v1/` and the handler strips the prefix when forwarding:

```rust
let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();
let upstream_path = format!("/{}{}", path, query);
state.proxy.forward(&upstream_path, method, body, content_type).await?;
forward_response(resp).await
```

**Why**: Copilot API uses direct paths like `/chat/completions`, not `/v1/chat/completions`.

### 2. Copilot Headers Required

**File**: `src/proxy.rs`

The upstream Copilot API rejects requests without specific headers. These masquerade as the VS Code Copilot extension:

- `editor-version: vscode/1.98.1`
- `editor-plugin-version: copilot-chat/0.26.7`
- `x-github-api-version: 2025-04-01`
- `copilot-integration-id: vscode-chat`

**DO NOT modify these headers** unless Copilot API changes.

### 3. Token Lifecycle

**File**: `src/auth.rs`

- Initial token fetch on `TokenManager::new()`
- Background refresh task spawns automatically via `spawn_refresh()`
- Refresh triggers at `refresh_in - 60` seconds
- Failed refresh retries after 30 seconds
- Graceful shutdown via `JoinHandle` abortion on `Drop`

**Thread Safety**: `Arc<RwLock<Option<CopilotToken>>>` allows concurrent reads from request handlers, exclusive writes during refresh.

### 4. Unified Response Forwarding

**File**: `src/proxy.rs`

Single function handles both streaming and non-streaming:

```rust
pub async fn forward_response(resp: reqwest::Response) -> Result<Response, Error> {
    let is_stream = headers.get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    let body = if is_stream {
        Body::from_stream(resp.bytes_stream())
    } else {
        Body::from(resp.bytes().await?)
    };
    // ...
}
```

**Why**: Copilot uses the same endpoint for both streaming and non-streaming; the response format depends on the request body.

### 5. Hop-by-Hop Header Filtering

**File**: `src/proxy.rs`

When forwarding responses, these headers are filtered out:
- `transfer-encoding`, `connection`, `keep-alive`
- `proxy-authenticate`, `proxy-authorization`, `te`, `trailers`, `upgrade`

**Why**: These headers are set by the HTTP stack and cause conflicts if manually forwarded.

### 6. Initiator Inference (Sticky Inference)

**Files**: `src/initiator.rs`, `src/server.rs`

- For `/v1/chat/completions` and `/v1/responses`, the proxy minimally parses the request body to check for any prior `assistant` or `tool` roles.
- If found, it sets `X-Initiator: agent` so follow-up turns do not consume Copilot premium requests.
- All other endpoints remain full passthrough.

### 7. Error Response Format

**File**: `src/error.rs`

Single `Error` enum with OpenAI-compatible responses:

```json
{
  "error": {
    "message": "...",
    "type": "authentication_error|config_error|upstream_error|invalid_request_error",
    "param": null,
    "code": null
  }
}
```

```rust
pub enum Error {
    Auth(String),
    Config(String),
    Upstream(#[from] reqwest::Error),
    InvalidRequest(String),
    Io(#[from] std::io::Error),
}
```

### 8. Token Storage

**File**: `src/config.rs`

- Path: `~/.local/share/copilot-api-proxy/github_token`
- Directory permissions: `0700` (owner only)
- File permissions: `0600` (owner read/write)

**Load priority**:
1. `GITHUB_TOKEN` environment variable
2. File at `~/.local/share/copilot-api-proxy/github_token`
3. Error if neither available

---

## Authentication Flow

### GitHub OAuth Device Flow

**File**: `src/auth.rs`

1. User runs `cargo run -- auth`
2. Proxy calls `POST https://github.com/login/device/code` with client ID `Iv1.b507a08c87ecfe98`
3. User receives verification URL and user code
4. User visits `github.com/login/device` and enters code
5. Proxy polls `POST https://github.com/login/oauth/access_token` every 5 seconds
6. GitHub returns access token
7. Token saved to `~/.local/share/copilot-api-proxy/github_token`

### Token Exchange

**File**: `src/auth.rs`

GitHub OAuth token → Copilot API token:

```
GET https://api.github.com/copilot_internal/v2/token
Authorization: Bearer {github_token}
```

Response:
```json
{
  "token": "copilot_api_token",
  "refresh_in": 7200
}
```

---

## Troubleshooting

### "Address already in use (os error 48)"

Port 9876 is already in use. Kill the existing process:

```bash
lsof -ti:9876 | xargs kill -9
```

### "GitHub token not found"

Run the OAuth flow:

```bash
cargo run -- auth
```

### Authentication fails

1. Remove old token: `rm ~/.local/share/copilot-api-proxy/github_token`
2. Re-authenticate: `cargo run -- auth`

### Upstream errors

Enable debug logging:

```bash
RUST_LOG=debug cargo run -- server
```

### Model not supported errors

Some models don't support certain endpoints:
- `/v1/responses` only works with `gpt-5` and similar reasoning models
- Use `/v1/chat/completions` for `gpt-4*` models
- Check available models with `curl http://localhost:9876/v1/models | jq '.data[].id'`
