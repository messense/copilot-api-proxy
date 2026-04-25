# AGENTS.md

## Project Overview

**copilot-api-proxy** is a Rust reverse proxy around GitHub Copilot with four route families:

1. OpenAI-compatible `/v1/*` routes that are forwarded nearly unchanged
2. Compatibility layers for Anthropic Messages and Gemini native APIs
3. Amp provider and management routes for Amp CLI / IDE clients
4. Droid LLM routes that are handled locally while Droid control-plane routes proxy to Factory by default

**Design Philosophy**

- Keep OpenAI-compatible routes as raw-byte passthrough whenever possible.
- Do protocol translation only on explicit compatibility surfaces for non-Claude models such as `/v1/messages`, `/v1/messages/count_tokens`, and Amp Anthropic / Gemini provider routes. Native Claude models on Anthropic routes are forwarded directly.
- Route ownership should stay explicit. `src/api.rs` owns the top-level `/api/*` split, while `src/amp/mod.rs` and `src/droid/mod.rs` own only their respective compatibility surfaces.
- Proxy Amp management traffic to `ampcode.com` by default. In `--amp-local` mode, serve the supported local `/api/*` subset, stub `/news.rss`, and fail loudly for unsupported Amp fallbacks instead of proxying them upstream.
- Handle Droid `/api/llm/*` routes locally through Copilot-compatible adapters in all modes. Proxy non-LLM Droid control-plane traffic to Factory by default. In `--droid-local` mode, serve the supported local control-plane subset and fail loudly for unsupported Droid fallbacks instead of proxying them upstream.
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

# Enable local Amp API handlers backed by local thread data
cargo run -- server --amp-local

# Enable local Droid control-plane handlers
cargo run -- server --droid-local

# Enable both local compatibility layers
cargo run -- server --amp-local --droid-local

# Local Amp mode with an explicit search backend
cargo run -- server --amp-local --search-provider jina

# Local Amp mode using Copilot Responses API web search
cargo run -- server --amp-local --search-provider model --search-model gpt-5-mini

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

# Override Droid / Factory control-plane upstream
export FACTORY_UPSTREAM_URL=https://api.factory.ai

# Override local Amp thread storage path
export AMP_THREADS_DIR=~/.local/share/amp/threads

# Search backend credentials for --amp-local
export JINA_API_KEY=your_jina_api_key
export TAVILY_API_KEY=your_tavily_api_key
export BRAVE_API_KEY=your_brave_api_key
export SEARXNG_URL=http://localhost:8080

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
    |      +-- /v1/messages -> native Claude passthrough to Copilot /v1/messages
    |      |                    or non-Claude conversion -> Copilot /chat/completions
    |      +-- /v1/messages/count_tokens -> native Claude passthrough or local tiktoken estimator
    |      '-- everything else -> generic Copilot passthrough
    |
    +-- /api/{*path}
    |      |
    |      +-- /api/provider/* -> Amp provider and management split
    |      +-- /api/llm/* -> Droid local LLM adapters
    |      +-- /api/cli/*, /api/organization/*, /api/sessions/*, /api/telemetry/* -> Droid control plane
    |      '-- remaining /api/* -> Amp management proxy or local Amp handlers
    |
    '-- Amp root routes
           |
           +-- /threads*, /auth*, /docs*, /settings* -> ampcode.com redirect/proxy behavior
           +-- /news.rss -> local stub when --amp-local is enabled
           '-- remaining unsupported Amp root routes -> loud 501 under --amp-local
```

### Key Architectural Decisions

1. **Generic OpenAI passthrough remains the default**. `/v1/{*path}` and Amp/Droid OpenAI-like routes forward raw bytes and avoid schema models where possible.
2. **Top-level API routing is centralized**. `src/api.rs` owns `/api/{*path}` dispatch and delegates to Amp or Droid modules.
3. **Compatibility logic is isolated**. Anthropic conversion lives in `src/claude.rs`; Gemini conversion lives in `src/gemini.rs`; shared LLM route handlers live in `src/llm.rs`; token estimation lives in `src/token_counter.rs`.
4. **Amp and Droid integrations are split cleanly**. `src/amp/mod.rs` owns Amp-specific provider and management behavior; `src/droid/mod.rs` owns Droid-specific behavior; local management subsets live in `src/amp/local.rs` and `src/droid/local.rs`.
5. **Sticky inference is opt-in by route**. Only chat/responses-style requests are inspected for `X-Initiator` and vision headers.
6. **Token refresh is background-managed**. `TokenManager` owns the Copilot token lifecycle and refreshes automatically.
7. **Response forwarding is unified**. `forward_response()` handles both buffered and SSE responses while stripping hop-by-hop headers.
8. **The server enforces a 10 MiB body limit** with `RequestBodyLimitLayer` and enables request tracing with `TraceLayer`.
9. **Local Amp mode is opt-in and strict**. `--amp-local` enables `src/amp/local.rs` for thread search, markdown export, internal RPCs, telemetry, labels, attachments, durable thread workers, and user info backed by local Amp data. It also serves a local `/news.rss` stub. Any other unsupported Amp fallback route returns a loud `501 Not Implemented` error instead of proxying upstream.
10. **Amp local web search is pluggable**. `--search-provider` selects `jina`, `tavily`, `brave`, `searxng`, `model`, or `none`; `--search-model` applies when the provider is `model` and defaults to `gpt-5-mini`.
11. **Droid local mode is opt-in and strict**. `--droid-local` enables `src/droid/local.rs` for `whoami`, managed settings, feature flags, local session index reads, session create/update writes (including `archive`/`unarchive`/`privacy`/`git-ai/checkpoints`), telemetry, daemon heartbeat, integrations probe, agent readiness reports (empty page), and LLM bookkeeping endpoints. Any other unsupported non-LLM Droid route returns a loud `501 Not Implemented` error instead of proxying upstream. **No Droid path ever falls through to `ampcode.com`** — every top-level `/api/*` segment claimed by `droid::matches_api_path` is owned by the Droid branch in all modes.

### Module Structure

```text
src/
├── api.rs           # Top-level /api router that dispatches to Amp or Droid
├── main.rs          # CLI entry point and service management
├── lib.rs           # Library exports
├── config.rs        # Token path, secure storage, environment loading
├── auth.rs          # GitHub device flow, token exchange, token manager
├── proxy.rs         # Copilot HTTP client and response forwarding
├── initiator.rs     # Sticky inference and vision detection
├── server.rs        # Main router, /v1 handler, Anthropic direct route handling
├── claude.rs        # Anthropic <-> OpenAI conversion, native Claude passthrough detection, and Anthropic-style errors
├── gemini.rs        # Gemini native API <-> OpenAI conversion
├── llm.rs           # Shared local OpenAI/Anthropic/Gemini route handlers
├── amp/
│   ├── mod.rs       # Amp provider routing and Amp-specific management proxy
│   └── local.rs     # Local Amp API handlers (--amp-local mode)
├── droid/
│   ├── mod.rs       # Droid LLM routing and Factory control-plane proxy
│   └── local.rs     # Local Droid control-plane handlers (--droid-local mode)
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

### 2. Anthropic Routes Use Native Passthrough For Claude Models

**Files**: `src/server.rs`, `src/claude.rs`, `src/amp/mod.rs`

When the request model is a native Claude model (name contains `claude`, `sonnet`, `haiku`, or `opus`), both `/v1/messages` and `/v1/messages/count_tokens` forward the Anthropic request directly to Copilot's corresponding `/v1/messages` endpoint without any OpenAI conversion. Initiator and vision flags are still inferred from the Anthropic message history.

For non-Claude models, the full translation layer applies:

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

**Files**: `src/amp/mod.rs`, `src/gemini.rs`

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

### 4. `/api/*` Routing Is Centralized

**Files**: `src/api.rs`, `src/amp/mod.rs`, `src/droid/mod.rs`

The top-level router no longer lets `src/amp/mod.rs` implicitly own every `/api/*` route. Instead:

- `src/api.rs` dispatches `/api/provider/*` and remaining non-Droid paths to Amp
- `src/api.rs` dispatches `/api/llm/*`, `/api/cli/*`, `/api/organization/*`, `/api/sessions/*`, and `/api/telemetry/*` to Droid
- exact `/api/telemetry` stays on the Amp side, while `/api/telemetry/*` belongs to Droid

This split keeps module ownership aligned with product ownership.

### 5. Amp Management Routes Default To `ampcode.com`, But `--amp-local` Is Strict

**File**: `src/amp/mod.rs`

By default, Amp management traffic is proxied to Amp upstream on these routes:

- `/api/*` when the path is not claimed by Droid and is not a supported local provider route
- `/threads`, `/threads/*`, `/threads.rss`
- `/news.rss`
- `/auth`, `/auth/*`
- `/docs`, `/docs/*`
- `/settings`, `/settings/*`

Auth handling for this proxy is separate from Copilot auth:

- `AMP_API_KEY` env var wins if set
- otherwise `~/.local/share/amp/secrets.json` is consulted
- when an upstream Amp API key is resolved, incoming auth headers are stripped and replaced

When `--amp-local` is enabled, these routes are handled locally:

- `/api/threads/find`
- `/api/threads/{id}.md`
- `/api/internal`
- `/api/telemetry`
- `/api/durable-thread-workers/*`
- `/api/users/*`
- `/api/attachments`
- `/news.rss` as a local stub feed

Those handlers use local Amp thread data from `AMP_THREADS_DIR` or `~/.local/share/amp/threads`.

Any other unsupported Amp fallback route returns `501 Not Implemented` with an `amp_local_unimplemented` error payload and an `amp_proxy` error log instead of proxying upstream.

### 5a. Amp Local Search Backends

**Files**: `src/main.rs`, `src/amp/local.rs`, `src/web_backend/*`

`--amp-local` supports pluggable web search/page extraction backends for internal RPC methods such as `webSearch2` and `extractWebPageContent`:

- `jina` uses Jina Search + Reader and optionally `JINA_API_KEY`
- `tavily` requires `TAVILY_API_KEY`
- `brave` requires `BRAVE_API_KEY` and falls back to Jina Reader for extraction
- `searxng` requires `SEARXNG_URL` and falls back to Jina Reader for extraction
- `model` uses Copilot `/v1/responses` with the `web_search` tool; `--search-model` defaults to `gpt-5-mini`
- `none` disables web search/page extraction

### 6. Amp Anthropic Requests Have One Cost-Saving Rewrite

**File**: `src/amp/mod.rs`

For Amp Anthropic provider traffic, native Claude models are normally forwarded via Copilot `/v1/messages`. However, lightweight non-streaming user-initiated `haiku` requests are excluded from native passthrough and instead rewritten to `gpt-5-mini` through the OpenAI conversion path. This is intentionally limited to the Amp provider path and is not applied to the direct `/v1/messages` route.

### 7. Droid Routing Uses Local LLM Adapters And Factory Control Plane

**Files**: `src/droid/mod.rs`, `src/droid/local.rs`, `src/llm.rs`

In all modes, these Droid LLM routes are handled locally:

- `/api/llm/o/v1/*`
- `/api/llm/a/v1/*`
- `/api/llm/g/v1/generate`

`droid::matches_api_path` claims every top-level `/api/*` segment the Droid CLI is known to call (verified against the `droid` v0.109.1 binary):

- `cli/*`, `feature-flags`, `organization/*`, `sessions/*`, `llm/*`, `telemetry/*`
- `daemon/*`, `hello`, `ingest`, `otlp/*`, `integrations/*`, `tools/*`, `v0/*`

Anything in this set is owned by the Droid branch and is **never** proxied to `ampcode.com`.

By default, non-LLM Droid routes are proxied to Factory upstream on:

- `/api/cli/*`, `/api/organization/*`, `/api/sessions/*`, `/api/telemetry/*`
- `/api/daemon/*`, `/api/hello`, `/api/ingest`, `/api/otlp/*`, `/api/integrations/*`, `/api/tools/*`
- `/api/v0/computers[...]`, `/api/v0/automations[...]`
- LLM bookkeeping such as `/api/llm/custom/usage` and `/api/llm/failed-requests`

When `--droid-local` is enabled, a strict local subset is served for:

- `GET  /api/cli/whoami`
- `GET  /api/sessions`
- `GET  /api/organization/managed-settings`
- `GET  /api/organization/agent-readiness-reports` (empty page)
- `GET  /api/feature-flags`
- `GET  /api/hello`
- `GET  /api/integrations/org/check`
- `POST /api/sessions/create`
- `POST /api/sessions/{id}/update-settings`
- `POST /api/sessions/{id}/message/create`
- `POST /api/sessions/{id}/update-title`
- `POST /api/sessions/{id}/droid-status`
- `POST /api/sessions/{id}/archive`, `/unarchive`, `/privacy`, `/git-ai/checkpoints`
- `POST /api/ingest`, `POST /api/otlp/traces/ingest`
- `POST /api/daemon/heartbeat`
- `POST /api/organization/agent-readiness-reports`
- `POST /api/llm/custom/usage`, `POST /api/llm/failed-requests`
- `POST /api/telemetry/cli-ingest`, `POST /api/telemetry/otlp/traces/ingest` *(legacy aliases)*

Unsupported non-LLM Droid routes return `501 Not Implemented` instead of proxying upstream under `--droid-local`.

### 8. Copilot Headers Are Mandatory

**File**: `src/proxy.rs`

Every upstream Copilot request injects headers that emulate the VS Code Copilot extension:

- `editor-version: vscode/1.114.0`
- `editor-plugin-version: copilot-chat/0.26.7`
- `user-agent: GitHubCopilotChat/0.26.7`
- `x-github-api-version: 2026-01-09`
- `copilot-integration-id: vscode-chat`
- `openai-intent: conversation-agent`

The proxy also sets:

- `X-Initiator: user|agent`
- `Copilot-Vision-Request: true` for vision inputs

### 9. Sticky Inference Is Minimal And Route-Aware

**Files**: `src/initiator.rs`, `src/server.rs`, `src/amp/mod.rs`

- OpenAI chat completions inspect `messages`
- OpenAI responses inspect `input`
- Anthropic requests infer from Anthropic message roles before conversion
- Gemini requests infer `agent` when prior `model` turns exist

Rules:

- Any prior `assistant` or `tool` turn marks the request as `agent`
- Otherwise it is `user`
- Invalid JSON falls back to `user`

Additionally, first-turn requests from automated agent systems are detected
via a two-condition check (identifying header + message content pattern) and
overridden to `agent` even when no prior assistant messages exist:

- **Factory/Droid**: `x-factory-client` header present AND message content
  contains task worker/subagent markers (`"You are a worker assigned to
  implement feature"`, `"## Worker Session"`, `"# Task Tool Invocation"`).
  Orchestrator sessions (no markers) stay `"user"`.

- **Amp subagents**: `x-amp-thread-id` header present AND system prompt
  contains subagent identity markers (`"You are the Oracle"`,
  `"You are a fast, parallel code search agent"`,
  `"You are a specialized subagent"`, `"You are the Librarian"`,
  `"You are the Walkthrough Planner"`, `"You are a REPL operator"`).
  The main Amp session (no subagent markers) stays `"user"`.

### 10. Token Lifecycle

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

### 11. Response Forwarding Filters Hop-By-Hop Headers

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

### 12. Error Shaping Depends On The Surface

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
editor-version: vscode/1.114.0
editor-plugin-version: copilot-chat/0.26.7
user-agent: GitHubCopilotChat/0.26.7
x-github-api-version: 2026-01-09
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

### `--amp-local` returns `501 Not Implemented` for Amp routes

That means the request hit an unsupported Amp fallback route. Only the documented local `/api/*` subset is implemented in `--amp-local`, plus a stub `/news.rss`. Other Amp management routes now fail loudly instead of proxying to `ampcode.com`.

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
- Anthropic compatibility routes use native Copilot `/v1/messages` for Claude models and Copilot chat completions for others
- Gemini compatibility routes ultimately target Copilot chat completions
- Use `curl http://localhost:9876/v1/models | jq '.data[].id'` to inspect the upstream model list
