//! Gemini native API ↔ OpenAI chat/completions conversion.
//!
//! Converts between Google's Gemini native API format (generateContent /
//! streamGenerateContent) and OpenAI's chat/completions format for routing
//! through Copilot.

use crate::error::Error;
use crate::token_counter::count_openai_tokens;
use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Result of converting a Gemini request to OpenAI format.
pub struct GeminiConvertedRequest {
    pub model: String,
    pub stream: bool,
    pub body: Bytes,
    pub initiator: &'static str,
    pub is_vision: bool,
}

/// Parse model name and method from Gemini API path.
///
/// Handles both standard and Amp publisher formats:
/// - `models/gemini-3-flash:streamGenerateContent`
/// - `publishers/google/models/gemini-3-flash:streamGenerateContent`
///
/// Returns `(model_name, method)` or `None`.
pub fn parse_gemini_action(path: &str) -> Option<(&str, &str)> {
    let models_idx = path.find("models/")?;
    let model_and_method = &path[models_idx + 7..];
    let colon_idx = model_and_method.find(':')?;
    let model = &model_and_method[..colon_idx];
    let method = &model_and_method[colon_idx + 1..];
    if model.is_empty() || method.is_empty() {
        return None;
    }
    Some((model, method))
}

// ---------------------------------------------------------------------------
// Token counting: Gemini countTokens
// ---------------------------------------------------------------------------

/// Handle Gemini countTokens by converting to OpenAI format and counting locally.
pub async fn handle_gemini_count_tokens(model: &str, body: Bytes) -> Result<Response, Error> {
    let value: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;

    let openai_body = convert_gemini_to_openai(&value, model, false)?;
    let total_tokens = count_openai_tokens(&openai_body).unwrap_or(1);

    let response_body = serde_json::json!({ "totalTokens": total_tokens });
    let json = serde_json::to_vec(&response_body)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize: {e}")))?;

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(json))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

// ---------------------------------------------------------------------------
// Request conversion: Gemini → OpenAI
// ---------------------------------------------------------------------------

/// Convert a Gemini generateContent/streamGenerateContent request to OpenAI
/// chat/completions format.
pub fn convert_gemini_request(
    model: &str,
    body: Bytes,
    stream: bool,
) -> Result<GeminiConvertedRequest, Error> {
    let value: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;

    let openai_body = convert_gemini_to_openai(&value, model, stream)?;

    // Infer initiator: agent if there are model turns (multi-turn conversation)
    let has_model_turn = value
        .get("contents")
        .and_then(|v| v.as_array())
        .map(|contents| {
            contents
                .iter()
                .any(|c| c.get("role").and_then(|v| v.as_str()) == Some("model"))
        })
        .unwrap_or(false);
    let initiator = if has_model_turn { "agent" } else { "user" };

    // Check for vision (inlineData parts)
    let is_vision = value
        .get("contents")
        .and_then(|v| v.as_array())
        .map(|contents| {
            contents.iter().any(|c| {
                c.get("parts")
                    .and_then(|v| v.as_array())
                    .is_some_and(|parts| parts.iter().any(|p| p.get("inlineData").is_some()))
            })
        })
        .unwrap_or(false);

    let body_bytes = serde_json::to_vec(&openai_body)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize: {e}")))?;

    Ok(GeminiConvertedRequest {
        model: model.to_string(),
        stream,
        body: Bytes::from(body_bytes),
        initiator,
        is_vision,
    })
}

fn convert_gemini_to_openai(value: &Value, model: &str, stream: bool) -> Result<Value, Error> {
    let mut messages = Vec::new();

    // System instruction → system message
    if let Some(system) = value.get("systemInstruction") {
        let text = extract_text_parts(system.get("parts"));
        if !text.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": text}));
        }
    }

    // Contents → messages
    if let Some(contents) = value.get("contents").and_then(|v| v.as_array()) {
        for content in contents {
            let role = content
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let Some(parts) = content.get("parts").and_then(|v| v.as_array()) else {
                continue;
            };
            match role {
                "user" => convert_user_parts(parts, &mut messages),
                "model" => convert_model_parts(parts, &mut messages),
                _ => {}
            }
        }
    }

    if messages.is_empty() {
        return Err(Error::InvalidRequest("No contents in request".to_string()));
    }

    let mut req = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });

    // generationConfig → OpenAI parameters
    if let Some(config) = value.get("generationConfig") {
        if let Some(v) = config.get("temperature") {
            req["temperature"] = v.clone();
        }
        if let Some(v) = config.get("topP") {
            req["top_p"] = v.clone();
        }
        if let Some(v) = config.get("maxOutputTokens") {
            req["max_tokens"] = v.clone();
        }
        if let Some(v) = config.get("stopSequences") {
            req["stop"] = v.clone();
        }
    }

    // tools → OpenAI tools
    if let Some(tools) = value.get("tools").and_then(|v| v.as_array()) {
        let openai_tools = convert_gemini_tools(tools);
        if !openai_tools.is_empty() {
            req["tools"] = Value::Array(openai_tools);
        }
    }

    // toolConfig → tool_choice
    if let Some(mode) = value
        .get("toolConfig")
        .and_then(|v| v.get("functionCallingConfig"))
        .and_then(|v| v.get("mode"))
        .and_then(|v| v.as_str())
    {
        req["tool_choice"] = Value::String(
            match mode {
                "ANY" => "required",
                "NONE" => "none",
                _ => "auto",
            }
            .to_string(),
        );
    }

    Ok(req)
}

fn convert_user_parts(parts: &[Value], messages: &mut Vec<Value>) {
    let mut text_parts = Vec::new();
    let mut func_responses = Vec::new();
    let mut image_parts = Vec::new();

    for part in parts {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            text_parts.push(text.to_string());
        }
        if let Some(func_resp) = part.get("functionResponse") {
            func_responses.push(func_resp.clone());
        }
        if let Some(inline_data) = part.get("inlineData") {
            let mime = inline_data
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png");
            let data = inline_data
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !data.is_empty() {
                image_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime};base64,{data}")}
                }));
            }
        }
    }

    // Function responses → tool messages
    for func_resp in &func_responses {
        let name = func_resp
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let response = func_resp
            .get("response")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let content_str = if response.is_string() {
            response.as_str().unwrap_or("").to_string()
        } else {
            serde_json::to_string(&response).unwrap_or_default()
        };
        messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": format!("call_{name}"),
            "content": content_str,
        }));
    }

    // Text + image content
    if !text_parts.is_empty() || !image_parts.is_empty() {
        if image_parts.is_empty() {
            messages.push(serde_json::json!({"role": "user", "content": text_parts.join("")}));
        } else {
            let mut content = Vec::new();
            if !text_parts.is_empty() {
                content.push(
                    serde_json::json!({"type": "text", "text": text_parts.join("")}),
                );
            }
            content.extend(image_parts);
            messages.push(serde_json::json!({"role": "user", "content": content}));
        }
    }
}

fn convert_model_parts(parts: &[Value], messages: &mut Vec<Value>) {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for part in parts {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            text_parts.push(text.to_string());
        }
        if let Some(func_call) = part.get("functionCall") {
            let name = func_call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = func_call
                .get("args")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            tool_calls.push(serde_json::json!({
                "id": format!("call_{name}"),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&args).unwrap_or_default()
                }
            }));
        }
    }

    let content = if text_parts.is_empty() {
        Value::Null
    } else {
        Value::String(text_parts.join(""))
    };

    let mut msg = serde_json::json!({"role": "assistant", "content": content});
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    messages.push(msg);
}

fn convert_gemini_tools(tools: &[Value]) -> Vec<Value> {
    let mut openai_tools = Vec::new();
    for tool in tools {
        let Some(decls) = tool
            .get("functionDeclarations")
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for decl in decls {
            let name = decl.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let description = decl
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parameters = decl
                .get("parameters")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            openai_tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            }));
        }
    }
    openai_tools
}

fn extract_text_parts(parts: Option<&Value>) -> String {
    let Some(parts) = parts.and_then(|v| v.as_array()) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Response conversion: OpenAI → Gemini
// ---------------------------------------------------------------------------

/// Convert an OpenAI response (streaming or non-streaming) back to Gemini format.
pub async fn convert_openai_to_gemini_response(
    resp: reqwest::Response,
    model: String,
    stream: bool,
) -> Result<Response, Error> {
    if !resp.status().is_success() {
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        let message = parse_openai_error(&bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
        let error_body = serde_json::json!({
            "error": {
                "code": status.as_u16(),
                "message": message,
                "status": match status.as_u16() {
                    400 => "INVALID_ARGUMENT",
                    401 | 403 => "PERMISSION_DENIED",
                    404 => "NOT_FOUND",
                    429 => "RESOURCE_EXHAUSTED",
                    _ => "INTERNAL",
                }
            }
        });
        let body = serde_json::to_vec(&error_body).unwrap_or_default();
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .map_err(|e| Error::InvalidRequest(e.to_string()));
    }

    if stream {
        return stream_openai_to_gemini(resp, model);
    }

    // Non-streaming
    let status = resp.status();
    let bytes = resp.bytes().await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::InvalidRequest(format!("Invalid OpenAI response: {e}")))?;
    let gemini = convert_openai_response_to_gemini(&value, &model);
    let body = serde_json::to_vec(&gemini)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize: {e}")))?;
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

fn convert_openai_response_to_gemini(value: &Value, model: &str) -> Value {
    let choice = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|v| v.first());

    let mut parts = Vec::new();

    if let Some(choice) = choice {
        if let Some(message) = choice.get("message") {
            // Text content
            if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(serde_json::json!({"text": text}));
                }
            }
            // Tool calls → functionCall parts
            if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some(func) = tc.get("function") {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args_str = func
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let args: Value =
                            serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                        parts
                            .push(serde_json::json!({"functionCall": {"name": name, "args": args}}));
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        parts.push(serde_json::json!({"text": ""}));
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(map_finish_reason)
        .unwrap_or("STOP");

    let usage = value.get("usage");
    let prompt = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    serde_json::json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": finish_reason,
        }],
        "usageMetadata": {
            "promptTokenCount": prompt,
            "candidatesTokenCount": completion,
            "totalTokenCount": prompt + completion,
        },
        "modelVersion": model,
    })
}

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}

fn parse_openai_error(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Streaming conversion: OpenAI SSE → Gemini SSE
// ---------------------------------------------------------------------------

fn stream_openai_to_gemini(
    resp: reqwest::Response,
    model: String,
) -> Result<Response, Error> {
    let status = resp.status();
    let upstream = resp.bytes_stream();
    let state = GeminiStreamState::new(upstream, model);

    let stream = futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(next) = state.pending.pop_front() {
                return Some((Ok(next), state));
            }
            if state.done {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(chunk)) => {
                    state.push_bytes(chunk);
                }
                Some(Err(err)) => {
                    state.done = true;
                    return Some((Err(std::io::Error::other(err)), state));
                }
                None => {
                    state.finish();
                }
            }
        }
    });

    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

struct ToolCallAccum {
    name: String,
    arguments: String,
}

struct GeminiStreamState<S> {
    upstream: S,
    buffer: String,
    pending: VecDeque<Bytes>,
    model: String,
    tool_calls: HashMap<usize, ToolCallAccum>,
    usage: Option<Value>,
    done: bool,
}

impl<S> GeminiStreamState<S> {
    fn new(upstream: S, model: String) -> Self {
        Self {
            upstream,
            buffer: String::new(),
            pending: VecDeque::new(),
            model,
            tool_calls: HashMap::new(),
            usage: None,
            done: false,
        }
    }

    fn push_bytes(&mut self, chunk: Bytes) {
        let Ok(text) = std::str::from_utf8(&chunk) else {
            return;
        };
        self.buffer.push_str(text);
        while let Some(pos) = self.buffer.find('\n') {
            let mut line = self.buffer[..pos].to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            self.buffer = self.buffer[pos + 1..].to_string();
            self.process_line(&line);
        }
    }

    fn process_line(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(data) = trimmed.strip_prefix("data:") else {
            return;
        };
        let payload = data.trim();
        if payload == "[DONE]" {
            self.flush_tool_calls();
            return;
        }

        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return;
        };

        // Capture usage
        if let Some(usage) = chunk.get("usage") {
            self.usage = Some(usage.clone());
        }

        let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) else {
            return;
        };
        let Some(choice) = choices.first() else {
            return;
        };

        // Accumulate tool calls (emitted together on finish)
        if let Some(delta) = choice.get("delta") {
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let entry = self
                        .tool_calls
                        .entry(index)
                        .or_insert_with(|| ToolCallAccum {
                            name: String::new(),
                            arguments: String::new(),
                        });
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            entry.name = name.to_string();
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }

            // Emit text deltas immediately
            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    let gemini_chunk = serde_json::json!({
                        "candidates": [{
                            "content": {
                                "parts": [{"text": content}],
                                "role": "model"
                            }
                        }],
                        "modelVersion": self.model,
                    });
                    self.pending
                        .push_back(Bytes::from(format!("data: {gemini_chunk}\n\n")));
                }
            }
        }

        // Handle finish
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            self.emit_finish(reason);
        }
    }

    fn flush_tool_calls(&mut self) {
        if self.tool_calls.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        let mut indices: Vec<_> = self.tool_calls.keys().cloned().collect();
        indices.sort();
        for idx in indices {
            if let Some(tc) = self.tool_calls.get(&idx) {
                let args: Value =
                    serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                parts.push(
                    serde_json::json!({"functionCall": {"name": tc.name, "args": args}}),
                );
            }
        }
        self.tool_calls.clear();

        let mut gemini_chunk = serde_json::json!({
            "candidates": [{
                "content": {"parts": parts, "role": "model"},
                "finishReason": "STOP",
            }],
            "modelVersion": self.model,
        });
        if let Some(usage) = &self.usage {
            gemini_chunk["usageMetadata"] = convert_usage(usage);
        }
        self.pending
            .push_back(Bytes::from(format!("data: {gemini_chunk}\n\n")));
    }

    fn emit_finish(&mut self, reason: &str) {
        // Emit any accumulated tool calls first
        self.flush_tool_calls();

        let mut gemini_chunk = serde_json::json!({
            "candidates": [{
                "content": {"parts": [], "role": "model"},
                "finishReason": map_finish_reason(reason),
            }],
            "modelVersion": self.model,
        });
        if let Some(usage) = &self.usage {
            gemini_chunk["usageMetadata"] = convert_usage(usage);
        }
        self.pending
            .push_back(Bytes::from(format!("data: {gemini_chunk}\n\n")));
    }

    fn finish(&mut self) {
        self.flush_tool_calls();
        self.done = true;
    }
}

fn convert_usage(usage: &Value) -> Value {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    serde_json::json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": completion,
        "totalTokenCount": prompt + completion,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gemini_action_standard() {
        let (model, method) =
            parse_gemini_action("models/gemini-3-flash:streamGenerateContent").unwrap();
        assert_eq!(model, "gemini-3-flash");
        assert_eq!(method, "streamGenerateContent");
    }

    #[test]
    fn test_parse_gemini_action_publisher() {
        let (model, method) = parse_gemini_action(
            "publishers/google/models/gemini-3-pro-preview:generateContent",
        )
        .unwrap();
        assert_eq!(model, "gemini-3-pro-preview");
        assert_eq!(method, "generateContent");
    }

    #[test]
    fn test_parse_gemini_action_invalid() {
        assert!(parse_gemini_action("models/").is_none());
        assert!(parse_gemini_action("no-models-here").is_none());
        assert!(parse_gemini_action("models/gemini").is_none()); // no colon
    }

    #[test]
    fn test_convert_simple_request() {
        let body = serde_json::json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "Hello"}]
            }],
            "generationConfig": {
                "temperature": 0.7,
                "maxOutputTokens": 1024
            }
        });
        let result =
            convert_gemini_request("gemini-3-flash", Bytes::from(body.to_string()), false)
                .unwrap();
        assert_eq!(result.model, "gemini-3-flash");
        assert!(!result.stream);
        assert_eq!(result.initiator, "user");

        let openai: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(openai["model"], "gemini-3-flash");
        assert_eq!(openai["messages"][0]["role"], "user");
        assert_eq!(openai["messages"][0]["content"], "Hello");
        assert_eq!(openai["temperature"], 0.7);
        assert_eq!(openai["max_tokens"], 1024);
    }

    #[test]
    fn test_convert_with_system_and_tools() {
        let body = serde_json::json!({
            "systemInstruction": {
                "parts": [{"text": "You are helpful."}]
            },
            "contents": [
                {"role": "user", "parts": [{"text": "Search for foo"}]},
                {"role": "model", "parts": [{"functionCall": {"name": "search", "args": {"q": "foo"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "search", "response": {"results": ["bar"]}}}]},
            ],
            "tools": [{
                "functionDeclarations": [{
                    "name": "search",
                    "description": "Search stuff",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
                }]
            }]
        });
        let result =
            convert_gemini_request("gemini-3-flash", Bytes::from(body.to_string()), true).unwrap();
        assert_eq!(result.initiator, "agent"); // has model turn

        let openai: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][1]["role"], "user");
        assert_eq!(openai["messages"][2]["role"], "assistant");
        assert!(openai["messages"][2]["tool_calls"].is_array());
        assert_eq!(openai["messages"][3]["role"], "tool");
        assert_eq!(openai["tools"][0]["function"]["name"], "search");
    }

    #[test]
    fn test_convert_response() {
        let openai_resp = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello there!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3
            }
        });
        let gemini = convert_openai_response_to_gemini(&openai_resp, "gemini-3-flash");
        assert_eq!(gemini["candidates"][0]["content"]["parts"][0]["text"], "Hello there!");
        assert_eq!(gemini["candidates"][0]["finishReason"], "STOP");
        assert_eq!(gemini["usageMetadata"]["promptTokenCount"], 5);
        assert_eq!(gemini["usageMetadata"]["candidatesTokenCount"], 3);
        assert_eq!(gemini["modelVersion"], "gemini-3-flash");
    }

    #[test]
    fn test_convert_tool_call_response() {
        let openai_resp = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"q\":\"foo\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let gemini = convert_openai_response_to_gemini(&openai_resp, "gemini-3-flash");
        let parts = &gemini["candidates"][0]["content"]["parts"];
        assert_eq!(parts[0]["functionCall"]["name"], "search");
        assert_eq!(parts[0]["functionCall"]["args"]["q"], "foo");
    }
}
