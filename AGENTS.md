# AGENTS.md

## Project Overview

**copilot-api-proxy** is a Rust reverse proxy around GitHub Copilot with three route families:

1. OpenAI-compatible `/v1/*` routes that are forwarded nearly unchanged
2. Compatibility layers for Anthropic Messages and Gemini native APIs
3. Amp provider and management routes for Amp CLI / IDE clients

**Design Philosophy**

- Keep OpenAI-compatible routes as raw-byte passthrough whenever possible.
- Do protocol translation only on explicit compatibility surfaces such as `/v1/messages`, `/v1/messages/count_tokens`, and Amp Anthropic / Gemini provider routes.
- Proxy Amp management traffic to `ampcode.com` unchanged except for optional API-key injection.
- Limit request inspection to the minimum needed for sticky inference and vision detection.

---

## Common Commands

### Build And Run

```bash
# Build the project
cargo build

# Build for release
cargo build --release

# Run the proxy server
cargo run -- server

# Run on custom port
cargo run -- server --port 8080

# Enable local Amp API mode (no ampcode.com dependency)
cargo run -- server --amp-local

# Increase log verbosity
cargo run -- server --log-level debug

# Run GitHub device-flow authentication
cargo run -- auth

# Install as a user service
cargo run -- service install

# Remove the user service
cargo run -- service uninstall
```

### Testing

```bash
# Run the full test suite
cargo test

# Run tests with output
cargo test -- --nocapture

# Run one test
cargo test test_name
```

### Manual Endpoint Checks

```bash
# OpenAI passthrough
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini-2024-07-18","messages":[{"role":"user","content":"Hello"}]}'

# Anthropic compatibility route
curl -X POST http://localhost:9876/v1/messages \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-20250514","max_tokens":512,"messages":[{"role":"user","content":"Hello"}]}'

# Anthropic token counting
curl -X POST http://localhost:9876/v1/messages/count_tokens \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":"Hello"}]}'

# Gemini provider route through Amp compatibility layer
curl -X POST \
  http://localhost:9876/api/provider/google/v1beta/models/gemini-2.5-pro:generateContent \
  -H "Content-Type: application/json" \
  -d '{"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}'

# List Copilot models via generic /v1 passthrough
curl http://localhost:9876/v1/models
```

### Environment Variables

```bash
# GitHub token (overrides file-based token)
export GITHUB_TOKEN=your_github_token

# Require an API key for the direct /v1/messages route
export ANTHROPIC_API_KEY=your-secret-key

# Override Amp management upstream
export AMP_UPSTREAM_URL=https://ampcode.com

# Override logging completely
export RUST_LOG=copilot_api_proxy=debug,tower_http=debug
```

---

## High-Level Architecture

### Request Flow

```text
Client Request
    |
    v
Axum Router
    |
    +-- /v1/{*path}
    |      |
    |      +-- /v1/messages -> Claude compatibility conversion -> Copilot /chat/completions
    |      +-- /v1/messages/count_tokens -> local tiktoken-based estimator
    |      '-- everything else -> generic Copilot passthrough
    |
    '-- /api/* and Amp root routes
           |
           +-- /api/provider/openai/* -> generic Copilot passthrough
           +-- /api/provider/anthropic/* -> Claude compatibility conversion
           +-- /api/provider/google/* -> Gemini compatibility conversion
           '-- other /api/*, /threads*, /auth*, /docs*, /settings* -> ampcode.com proxy
                (or local handlers when --amp-local is enabled)
```

### Key Architectural Decisions

1. **Generic OpenAI passthrough remains the default**. `/v1/{*path}` and Amp OpenAI provider routes forward raw bytes and avoid schema models.
2. **Compatibility logic is isolated**. Anthropic conversion lives in `src/claude.rs`; Gemini conversion lives in `src/gemini.rs`; token estimation lives in `src/token_counter.rs`.
3. **Amp integration is split cleanly**. Provider routes are handled locally when supported, while management traffic is proxied to Amp upstream.
4. **Sticky inference is opt-in by route**. Only chat/responses-style requests are inspected for `X-Initiator` and vision headers.
5. **Token refresh is background-managed**. `TokenManager` owns the Copilot token lifecycle and refreshes automatically.
6. **Response forwarding is unified**. `forward_response()` handles both buffered and SSE responses while stripping hop-by-hop headers.
7. **The server enforces a 10 MiB body limit** with `RequestBodyLimitLayer` and enables request tracing with `TraceLayer`.
8. **Local Amp mode is opt-in**. `--amp-local` enables `src/amp_local.rs` which serves thread search, markdown export, telemetry, labels, and user info from local `~/.local/share/amp/threads/` data instead of proxying to ampcode.com.

### Module Structure

```text
src/
├── main.rs          # CLI entry point and service management
├── lib.rs           # Library exports
├── config.rs        # Token path, secure storage, environment loading
├── auth.rs          # GitHub device flow, token exchange, token manager
├── proxy.rs         # Copilot HTTP client and response forwarding
├── initiator.rs     # Sticky inference and vision detection
├── server.rs        # Main router, /v1 handler, Anthropic direct route handling
├── claude.rs        # Anthropic <-> OpenAI conversion and Anthropic-style errors
├── gemini.rs        # Gemini native API <-> OpenAI conversion
├── amp.rs           # Amp provider routing and management reverse proxy
├── amp_local.rs     # Local Amp API handlers (--amp-local mode)
├── token_counter.rs # Local token estimation for Anthropic and Gemini routes
├── error.rs         # Shared internal error enum with OpenAI-style responses
└── web_backend/     # Amp web backend components for local mode
```

---

## Critical Implementation Details

### 1. `/v1/{*path}` Is A Generic Copilot Passthrough

**Files**: `src/server.rs`, `src/proxy.rs`

The main router captures every `/v1/*` path. The handler strips the `/v1/` prefix and forwards the remainder to `https://api.individual.githubcopilot.com`.

```rust
let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();
state
    .proxy
    .forward(
        &format!("/{}{}", path, query),
        method,
        body,
        content_type,
        analysis.map(|a| a.initiator),
        analysis.map(|a| a.is_vision).unwrap_or(false),
    )
    .await?;
```

Only two `/v1` paths are special-cased locally:

- `/v1/messages`
- `/v1/messages/count_tokens`

Everything else remains generic passthrough.

### 2. Anthropic Compatibility Is A Real Translation Layer

**Files**: `src/server.rs`, `src/claude.rs`

`/v1/messages` is not a raw proxy. It:

1. Optionally checks `ANTHROPIC_API_KEY`
2. Converts the Anthropic request into OpenAI chat/completions format
3. Infers initiator and vision flags from Anthropic message history
4. Sends the converted request to Copilot `/chat/completions`
5. Converts the OpenAI response back into Anthropic format, including SSE streaming

Anthropic model aliases are mapped by substring:

- `haiku` -> `SMALL_MODEL`
- `sonnet` -> `MIDDLE_MODEL`
- `opus` -> `BIG_MODEL`

`max_tokens` is clamped to `MIN_TOKENS_LIMIT..=MAX_TOKENS_LIMIT`.

### 3. Gemini Support Lives On Amp Provider Routes

**Files**: `src/amp.rs`, `src/gemini.rs`

Gemini native API requests are handled on Amp-style provider paths such as:

- `/api/provider/google/v1beta/models/{model}:generateContent`
- `/api/provider/google/v1beta/models/{model}:streamGenerateContent`
- `/api/provider/google/v1beta/models/{model}:countTokens`

The parser also accepts publisher-style paths like `publishers/google/models/...` after the version prefix.

The flow is:

1. Convert Gemini `contents`, tools, and generation config to OpenAI chat/completions
2. Forward to Copilot `/chat/completions`
3. Convert buffered or streaming responses back to Gemini format
4. Estimate token counts locally for `countTokens`

### 4. Amp Management Routes Proxy To `ampcode.com`

**File**: `src/amp.rs`

Amp management traffic is not handled locally. The proxy forwards these routes to Amp upstream:

- `/api/*` when the path is not a supported local provider route
- `/threads`, `/threads/*`, `/threads.rss`
- `/news.rss`
- `/auth`, `/auth/*`
- `/docs`, `/docs/*`
- `/settings`, `/settings/*`

Auth handling for this proxy is separate from Copilot auth:

- `AMP_API_KEY` env var wins if set
- otherwise `~/.local/share/amp/secrets.json` is consulted
- when an upstream Amp API key is resolved, incoming auth headers are stripped and replaced

### 5. Amp Anthropic Requests Have One Cost-Saving Rewrite

**File**: `src/amp.rs`

For Amp Anthropic provider traffic, lightweight non-streaming user-initiated `haiku` requests are rewritten to `gpt-5-mini` before forwarding. This is intentionally limited to that path and is not applied to the direct `/v1/messages` route.

### 6. Copilot Headers Are Mandatory

**File**: `src/proxy.rs`

Every upstream Copilot request injects headers that emulate the VS Code Copilot extension:

- `editor-version: vscode/1.98.1`
- `editor-plugin-version: copilot-chat/0.26.7`
- `user-agent: GitHubCopilotChat/0.26.7`
- `x-github-api-version: 2025-04-01`
- `copilot-integration-id: vscode-chat`
- `openai-intent: conversation-panel`

The proxy also sets:

- `X-Initiator: user|agent`
- `Copilot-Vision-Request: true` for vision inputs

### 7. Sticky Inference Is Minimal And Route-Aware

**Files**: `src/initiator.rs`, `src/server.rs`, `src/amp.rs`

- OpenAI chat completions inspect `messages`
- OpenAI responses inspect `input`
- Anthropic requests infer from Anthropic message roles before conversion
- Gemini requests infer `agent` when prior `model` turns exist

Rules:

- Any prior `assistant` or `tool` turn marks the request as `agent`
- Otherwise it is `user`
- Invalid JSON falls back to `user`

### 8. Token Lifecycle

**Files**: `src/auth.rs`, `src/config.rs`

GitHub token loading order:

1. `GITHUB_TOKEN` environment variable
2. `~/.local/share/copilot-api-proxy/github_token`
3. Error if neither exists

Copilot token lifecycle:

1. `TokenManager::new()` exchanges the GitHub token immediately
2. The result is stored in `Arc<RwLock<Option<CopilotToken>>>`
3. A background task refreshes at `refresh_in - 60` seconds, clamped to at least 1 second
4. Failed refreshes are retried after 30 seconds
5. `Drop` aborts the refresh task

The token exchange request uses:

```text
GET https://api.github.com/copilot_internal/v2/token
Authorization: token {github_token}
```

### 9. Response Forwarding Filters Hop-By-Hop Headers

**File**: `src/proxy.rs`

`forward_response()` removes these headers before returning to the client:

- `transfer-encoding`
- `connection`
- `keep-alive`
- `proxy-authenticate`
- `proxy-authorization`
- `te`
- `trailers`
- `upgrade`

If the upstream response is SSE and lacks `cache-control`, the proxy adds `Cache-Control: no-cache`.

### 10. Error Shaping Depends On The Surface

**Files**: `src/error.rs`, `src/claude.rs`, `src/gemini.rs`

- Internal errors returned from generic handlers use an OpenAI-style `{ "error": ... }` envelope.
- Anthropic compatibility routes reshape upstream and local errors into Anthropic-style error payloads.
- Gemini compatibility routes reshape upstream and local errors into Gemini-style `{ "error": { code, message, status } }` payloads.

---

## Authentication Flow

### GitHub OAuth Device Flow

**File**: `src/auth.rs`

1. User runs `cargo run -- auth`
2. The proxy calls `POST https://github.com/login/device/code` with client ID `Iv1.b507a08c87ecfe98`
3. The CLI prints the verification URL and user code
4. The user visits `github.com/login/device` and enters the code
5. The proxy polls `POST https://github.com/login/oauth/access_token`
6. GitHub returns an access token
7. The token is stored at `~/.local/share/copilot-api-proxy/github_token` with mode `0600`

### Copilot Token Exchange

**File**: `src/auth.rs`

GitHub OAuth token -> Copilot API token:

```text
GET https://api.github.com/copilot_internal/v2/token
Authorization: token {github_token}
Accept: application/json
editor-version: vscode/1.98.1
editor-plugin-version: copilot-chat/0.26.7
user-agent: GitHubCopilotChat/0.26.7
x-github-api-version: 2025-04-01
```

Expected response shape:

```json
{
  "token": "copilot_api_token",
  "refresh_in": 7200
}
```

---

## Troubleshooting

### `Address already in use (os error 48)`

Port `9876` is already in use:

```bash
lsof -ti:9876 | xargs kill -9
```

### `GitHub token not found`

Run the device flow again:

```bash
cargo run -- auth
```

### Authentication fails

1. Remove `~/.local/share/copilot-api-proxy/github_token`
2. Re-run `cargo run -- auth`

### Anthropic `/v1/messages` returns unauthorized

If `ANTHROPIC_API_KEY` is set, clients must send the same key via `x-api-key` or `Authorization: Bearer ...`.

### Amp management routes fail with upstream auth errors

Set `AMP_API_KEY` or make sure `~/.local/share/amp/secrets.json` contains a valid Amp API key.

### Upstream errors

Increase logging:

```bash
cargo run -- server --log-level debug
```

or

```bash
RUST_LOG=copilot_api_proxy=debug,tower_http=debug cargo run -- server
```

### Model-specific errors

- `/v1/responses` depends on the selected upstream model supporting the Responses API
- Anthropic compatibility routes ultimately target Copilot chat completions
- Gemini compatibility routes ultimately target Copilot chat completions
- Use `curl http://localhost:9876/v1/models | jq '.data[].id'` to inspect the upstream model list
