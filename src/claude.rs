//! Claude Code compatibility: convert /v1/messages to OpenAI chat completions.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{error::Error, initiator::infer_initiator_claude as infer_initiator};

const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";
const ROLE_SYSTEM: &str = "system";
const ROLE_TOOL: &str = "tool";

const CONTENT_TEXT: &str = "text";
const CONTENT_IMAGE: &str = "image";
const CONTENT_TOOL_USE: &str = "tool_use";
const CONTENT_TOOL_RESULT: &str = "tool_result";

const TOOL_FUNCTION: &str = "function";

const STOP_END_TURN: &str = "end_turn";
const STOP_MAX_TOKENS: &str = "max_tokens";
const STOP_TOOL_USE: &str = "tool_use";

const EVENT_MESSAGE_START: &str = "message_start";
const EVENT_MESSAGE_STOP: &str = "message_stop";
const EVENT_MESSAGE_DELTA: &str = "message_delta";
const EVENT_CONTENT_BLOCK_START: &str = "content_block_start";
const EVENT_CONTENT_BLOCK_STOP: &str = "content_block_stop";
const EVENT_CONTENT_BLOCK_DELTA: &str = "content_block_delta";
const EVENT_PING: &str = "ping";

const DELTA_TEXT: &str = "text_delta";
const DELTA_INPUT_JSON: &str = "input_json_delta";

pub struct ClaudeConvertedRequest {
    pub model: String,
    pub stream: bool,
    pub body: Bytes,
    pub initiator: String, // "user" or "agent"
    pub is_vision: bool,
}

pub struct ClaudeRequestMetadata {
    pub model: String,
    pub stream: bool,
    pub initiator: String, // "user" or "agent"
    pub is_vision: bool,
}

pub fn analyze_claude_request(
    body: &[u8],
    headers: Option<&HeaderMap>,
) -> Result<ClaudeRequestMetadata, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;
    analyze_claude_value(&value, headers)
}

pub fn extract_anthropic_model(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("model")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn is_native_claude_model(model: &str) -> bool {
    let model = model.to_lowercase();
    model.contains("claude")
        || model.contains("sonnet")
        || model.contains("haiku")
        || model.contains("opus")
}

pub fn validate_anthropic_headers(headers: &HeaderMap) -> Option<Response> {
    let expected = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let provided = extract_client_api_key(headers);
    if provided.as_deref() == Some(expected.as_str()) {
        return None;
    }

    Some(error_response(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Invalid API key. Please provide a valid Anthropic API key.",
    ))
}

pub fn convert_claude_request(
    body: Bytes,
    headers: Option<&HeaderMap>,
) -> Result<ClaudeConvertedRequest, Error> {
    let value: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;

    let metadata = analyze_claude_value(&value, headers)?;

    let openai_body = convert_claude_value_to_openai(&value)?;
    let body_bytes = serde_json::to_vec(&openai_body)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize OpenAI request: {e}")))?;

    Ok(ClaudeConvertedRequest {
        model: metadata.model,
        stream: metadata.stream,
        body: Bytes::from(body_bytes),
        initiator: metadata.initiator,
        is_vision: metadata.is_vision,
    })
}

fn analyze_claude_value(
    value: &Value,
    headers: Option<&HeaderMap>,
) -> Result<ClaudeRequestMetadata, Error> {
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidRequest("Missing required field: model".to_string()))?
        .to_string();

    let stream = value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let messages = value
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidRequest("Missing required field: messages".to_string()))?;
    let initiator = infer_initiator(messages, headers).to_string();

    let is_vision = messages.iter().any(|msg| {
        msg.get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .any(|p| p.get("type").and_then(|t| t.as_str()) == Some(CONTENT_IMAGE))
            })
            .unwrap_or(false)
    });

    Ok(ClaudeRequestMetadata {
        model,
        stream,
        initiator,
        is_vision,
    })
}

pub async fn convert_openai_response(
    resp: reqwest::Response,
    original_model: String,
    stream: bool,
) -> Result<Response, Error> {
    if !resp.status().is_success() {
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        let message = parse_upstream_error(&bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
        let message = if message.is_empty() {
            "Upstream error".to_string()
        } else {
            message
        };
        let error_type = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            StatusCode::BAD_REQUEST => "invalid_request_error",
            _ => "api_error",
        };
        return Ok(error_response(status, error_type, &message));
    }

    if stream {
        return stream_openai_to_claude(resp, original_model);
    }

    let status = resp.status();
    let bytes = resp.bytes().await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::InvalidRequest(format!("Invalid OpenAI response JSON: {e}")))?;
    let claude = convert_openai_to_claude_response(&value, &original_model)?;
    let body = serde_json::to_vec(&claude)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize Claude response: {e}")))?;
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

pub fn error_from_proxy(err: Error) -> Response {
    let (status, error_type) = match &err {
        Error::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication_error"),
        Error::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error"),
        Error::Upstream(_) => (StatusCode::BAD_GATEWAY, "api_error"),
        Error::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        Error::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };
    error_response(status, error_type, &err.to_string())
}

fn convert_claude_value_to_openai(value: &Value) -> Result<Value, Error> {
    let openai_model = map_model(
        value
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    );

    let max_tokens = value.get("max_tokens").and_then(|v| v.as_u64());
    let temperature = value.get("temperature").and_then(|v| v.as_f64());
    let top_p = value.get("top_p").and_then(|v| v.as_f64());
    let stop_sequences = value.get("stop_sequences").and_then(|v| v.as_array());
    let stream = value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut openai_messages = Vec::new();

    if let Some(system) = value.get("system")
        && let Some(text) = system_text(system)
        && !text.is_empty()
    {
        openai_messages.push(serde_json::json!({
            "role": ROLE_SYSTEM,
            "content": text,
        }));
    }

    let messages = value
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidRequest("Missing required field: messages".to_string()))?;

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == ROLE_USER {
            openai_messages.push(convert_claude_user_message(msg));
        } else if role == ROLE_ASSISTANT {
            openai_messages.push(convert_claude_assistant_message(msg));

            if i + 1 < messages.len() {
                let next_msg = &messages[i + 1];
                let next_role = next_msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if next_role == ROLE_USER && contains_tool_result(next_msg) {
                    let tool_results = convert_claude_tool_results(next_msg);
                    openai_messages.extend(tool_results);
                    i += 1;
                }
            }
        }
        i += 1;
    }

    let mut openai_request = serde_json::json!({
        "model": openai_model,
        "messages": openai_messages,
        "stream": stream,
    });

    if let Some(max_tokens) = clamp_max_tokens(max_tokens) {
        openai_request["max_tokens"] = Value::from(max_tokens);
    }
    if let Some(temperature) = temperature {
        openai_request["temperature"] = Value::from(temperature);
    }
    if let Some(top_p) = top_p {
        openai_request["top_p"] = Value::from(top_p);
    }
    if let Some(stop_sequences) = stop_sequences {
        openai_request["stop"] = Value::Array(stop_sequences.clone());
    }

    if let Some(tools) = value.get("tools").and_then(|v| v.as_array()) {
        let mut openai_tools = Vec::new();
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parameters = tool.get("input_schema").cloned().unwrap_or(Value::Null);
            openai_tools.push(serde_json::json!({
                "type": TOOL_FUNCTION,
                TOOL_FUNCTION: {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            }));
        }
        if !openai_tools.is_empty() {
            openai_request["tools"] = Value::Array(openai_tools);
        }
    }

    if let Some(tool_choice) = value.get("tool_choice").and_then(|v| v.as_object()) {
        let choice_type = tool_choice
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if choice_type == "tool" {
            if let Some(name) = tool_choice.get("name").and_then(|v| v.as_str()) {
                openai_request["tool_choice"] = serde_json::json!({
                    "type": TOOL_FUNCTION,
                    TOOL_FUNCTION: { "name": name }
                });
            }
        } else {
            openai_request["tool_choice"] = Value::String("auto".to_string());
        }
    }

    Ok(openai_request)
}

fn convert_claude_user_message(msg: &Value) -> Value {
    let content = msg.get("content");
    if let Some(text) = content.and_then(|v| v.as_str()) {
        return serde_json::json!({ "role": ROLE_USER, "content": text });
    }

    let mut openai_content = Vec::new();
    if let Some(blocks) = content.and_then(|v| v.as_array()) {
        for block in blocks {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type == CONTENT_TEXT {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    openai_content.push(serde_json::json!({ "type": "text", "text": text }));
                }
            } else if block_type == CONTENT_IMAGE
                && let Some(source) = block.get("source").and_then(|v| v.as_object())
                && source.get("type").and_then(|v| v.as_str()) == Some("base64")
            {
                let media_type = source
                    .get("media_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                if !media_type.is_empty() && !data.is_empty() {
                    openai_content.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{};base64,{}", media_type, data) }
                    }));
                }
            }
        }
    }

    if openai_content.len() == 1
        && openai_content[0].get("type").and_then(|v| v.as_str()) == Some("text")
        && let Some(text) = openai_content[0].get("text").and_then(|v| v.as_str())
    {
        return serde_json::json!({ "role": ROLE_USER, "content": text });
    }

    serde_json::json!({ "role": ROLE_USER, "content": openai_content })
}

fn convert_claude_assistant_message(msg: &Value) -> Value {
    let content = msg.get("content");
    if let Some(text) = content.and_then(|v| v.as_str()) {
        return serde_json::json!({ "role": ROLE_ASSISTANT, "content": text });
    }

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(blocks) = content.and_then(|v| v.as_array()) {
        for block in blocks {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type == CONTENT_TEXT {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(text);
                }
            } else if block_type == CONTENT_TOOL_USE {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": TOOL_FUNCTION,
                    TOOL_FUNCTION: {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
        }
    }

    let content_value = if text_parts.is_empty() {
        Value::Null
    } else {
        Value::String(text_parts.join(""))
    };

    let mut message = serde_json::json!({
        "role": ROLE_ASSISTANT,
        "content": content_value,
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    message
}

fn contains_tool_result(msg: &Value) -> bool {
    msg.get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(|t| t == CONTENT_TOOL_RESULT)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn convert_claude_tool_results(msg: &Value) -> Vec<Value> {
    let mut results = Vec::new();
    if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some(CONTENT_TOOL_RESULT) {
                continue;
            }
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = parse_tool_result_content(block.get("content"));
            results.push(serde_json::json!({
                "role": ROLE_TOOL,
                "tool_call_id": tool_use_id,
                "content": content,
            }));
        }
    }
    results
}

fn parse_tool_result_content(content: Option<&Value>) -> String {
    match content {
        None => "No content provided".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                } else if let Some(s) = item.as_str() {
                    parts.push(s.to_string());
                } else {
                    parts.push(item.to_string());
                }
            }
            parts.join("\n").trim().to_string()
        }
        Some(Value::Object(obj)) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some(CONTENT_TEXT) {
                obj.get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                Value::Object(obj.clone()).to_string()
            }
        }
        Some(other) => other.to_string(),
    }
}

fn system_text(system: &Value) -> Option<String> {
    let raw = if let Some(text) = system.as_str() {
        text.to_string()
    } else if let Some(blocks) = system.as_array() {
        let mut parts = Vec::new();
        for block in blocks {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type == CONTENT_TEXT
                && let Some(text) = block.get("text").and_then(|v| v.as_str())
            {
                parts.push(text.to_string());
            }
        }
        parts.join("\n\n").trim().to_string()
    } else {
        return None;
    };
    Some(sanitize_system_prompt(&raw))
}

fn sanitize_system_prompt(text: &str) -> String {
    text.lines()
        .filter(|line| !line.contains("x-anthropic-"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_model(model: &str) -> String {
    let model_lower = model.to_lowercase();
    let big = std::env::var("BIG_MODEL").unwrap_or_else(|_| "claude-opus-4.5".to_string());
    let middle = std::env::var("MIDDLE_MODEL").unwrap_or_else(|_| "claude-sonnet-4.5".to_string());
    let small = std::env::var("SMALL_MODEL").unwrap_or_else(|_| "claude-haiku-4.5".to_string());

    if model_lower.contains("haiku") {
        small
    } else if model_lower.contains("sonnet") {
        middle
    } else if model_lower.contains("opus") {
        big
    } else {
        model.to_string()
    }
}

fn clamp_max_tokens(value: Option<u64>) -> Option<u64> {
    let max_limit = std::env::var("MAX_TOKENS_LIMIT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4096);
    let min_limit = std::env::var("MIN_TOKENS_LIMIT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100);
    value.map(|v| v.clamp(min_limit, max_limit))
}

fn convert_openai_to_claude_response(value: &Value, original_model: &str) -> Result<Value, Error> {
    let choices = value
        .get("choices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidRequest("OpenAI response missing choices".to_string()))?;
    let choice = choices
        .first()
        .ok_or_else(|| Error::InvalidRequest("OpenAI response has no choices".to_string()))?;
    let mut content_blocks = Vec::new();

    if let Some(message) = choice.get("message").and_then(|v| v.as_object()) {
        if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
            content_blocks.push(serde_json::json!({ "type": CONTENT_TEXT, "text": text }));
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for tool_call in tool_calls {
                if tool_call.get("type").and_then(|v| v.as_str()) != Some(TOOL_FUNCTION) {
                    continue;
                }
                let function = tool_call.get(TOOL_FUNCTION).and_then(|v| v.as_object());
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let arguments = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let input = serde_json::from_str::<Value>(arguments)
                    .unwrap_or_else(|_| Value::Object(Default::default()));
                let id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let tool_id = if id.is_empty() {
                    format!("tool_{}", make_id())
                } else {
                    id.to_string()
                };
                content_blocks.push(serde_json::json!({
                    "type": CONTENT_TOOL_USE,
                    "id": tool_id,
                    "name": name,
                    "input": input,
                    "signature": "",
                }));
            }
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(serde_json::json!({ "type": CONTENT_TEXT, "text": "" }));
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop");
    let stop_reason = match finish_reason {
        "length" => STOP_MAX_TOKENS,
        "tool_calls" | "function_call" => STOP_TOOL_USE,
        _ => STOP_END_TURN,
    };

    let usage = value.get("usage").and_then(|v| v.as_object());
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let response_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|id| id.starts_with("msg_"))
        .map(|id| id.to_string())
        .unwrap_or_else(|| format!("msg_{}", make_id()));

    Ok(serde_json::json!({
        "id": response_id,
        "type": "message",
        "role": ROLE_ASSISTANT,
        "model": original_model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}

fn stream_openai_to_claude(
    resp: reqwest::Response,
    original_model: String,
) -> Result<Response, Error> {
    let status = resp.status();
    let upstream = resp.bytes_stream();
    let state = StreamState::new(upstream, original_model);
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
                    let io_err = std::io::Error::other(err);
                    return Some((Err(io_err), state));
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
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn analyze_claude_request_sets_user_initiator() {
        let body = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });

        let metadata = analyze_claude_request(body.to_string().as_bytes(), None).unwrap();
        assert_eq!(metadata.initiator, "user");
        assert!(!metadata.is_vision);
    }

    #[test]
    fn analyze_claude_request_sets_agent_initiator() {
        let body = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"},
                {"role": "user", "content": "Continue"}
            ]
        });

        let metadata = analyze_claude_request(body.to_string().as_bytes(), None).unwrap();
        assert_eq!(metadata.initiator, "agent");
        assert!(!metadata.is_vision);
    }

    #[test]
    fn analyze_claude_request_detects_vision() {
        let body = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "What is this?"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                    ]
                }
            ]
        });

        let metadata = analyze_claude_request(body.to_string().as_bytes(), None).unwrap();
        assert_eq!(metadata.initiator, "user");
        assert!(metadata.is_vision);
    }
}

fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message,
        }
    });
    let json = serde_json::to_vec(&body)
        .unwrap_or_else(|_| b"{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"serialization error\"}}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(json))
        .unwrap_or_else(|_| Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"response build error\"}}"))
            .unwrap())
}

fn extract_client_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if value.len() >= 7 && value[..7].eq_ignore_ascii_case("bearer ") {
            let token = value[7..].trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

fn parse_upstream_error(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let message = value
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str());
    message.map(|m| m.to_string())
}

struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    args_buffer: String,
    claude_index: Option<usize>,
    started: bool,
}

struct StreamState<S> {
    upstream: S,
    buffer: String,
    pending: VecDeque<Bytes>,
    converter: StreamConverter,
    done: bool,
}

impl<S> StreamState<S> {
    fn new(upstream: S, original_model: String) -> Self {
        let mut converter = StreamConverter::new(original_model);
        let mut pending = VecDeque::new();
        converter.start(&mut pending);
        Self {
            upstream,
            buffer: String::new(),
            pending,
            converter,
            done: false,
        }
    }

    fn push_bytes(&mut self, chunk: Bytes) {
        match std::str::from_utf8(&chunk) {
            Ok(text) => {
                self.buffer.push_str(text);
                while let Some(pos) = self.buffer.find('\n') {
                    let mut line = self.buffer[..pos].to_string();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    self.buffer = self.buffer[pos + 1..].to_string();
                    self.converter.consume_line(&line, &mut self.pending);
                }
            }
            Err(err) => {
                self.pending.push_back(Bytes::from(format!(
                    "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"Invalid UTF-8 in stream: {err}\"}}}}\n\n"
                )));
                self.finish();
            }
        }
    }

    fn finish(&mut self) {
        if !self.converter.finished {
            self.converter.finish(&mut self.pending);
        }
        self.done = true;
    }
}

struct StreamConverter {
    original_model: String,
    message_id: String,
    text_block_index: usize,
    tool_block_counter: usize,
    current_tool_calls: HashMap<usize, ToolCallState>,
    final_stop_reason: &'static str,
    usage: Value,
    finished: bool,
}

impl StreamConverter {
    fn new(original_model: String) -> Self {
        Self {
            original_model,
            message_id: format!("msg_{}", make_id()),
            text_block_index: 0,
            tool_block_counter: 0,
            current_tool_calls: HashMap::new(),
            final_stop_reason: STOP_END_TURN,
            usage: serde_json::json!({ "input_tokens": 0, "output_tokens": 0 }),
            finished: false,
        }
    }

    fn start(&mut self, pending: &mut VecDeque<Bytes>) {
        let message_start = serde_json::json!({
            "type": EVENT_MESSAGE_START,
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": ROLE_ASSISTANT,
                "model": self.original_model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_MESSAGE_START}\ndata: {}\n\n",
            message_start
        )));

        let content_block_start = serde_json::json!({
            "type": EVENT_CONTENT_BLOCK_START,
            "index": self.text_block_index,
            "content_block": { "type": CONTENT_TEXT, "text": "" }
        });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_CONTENT_BLOCK_START}\ndata: {}\n\n",
            content_block_start
        )));

        let ping = serde_json::json!({ "type": EVENT_PING });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_PING}\ndata: {}\n\n",
            ping
        )));
    }

    fn consume_line(&mut self, line: &str, pending: &mut VecDeque<Bytes>) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(data) = trimmed.strip_prefix("data:") {
            let payload = data.trim();
            if payload == "[DONE]" {
                self.finish(pending);
                return;
            }
            let chunk: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => return,
            };

            if let Some(usage) = chunk.get("usage")
                && let Some(prompt_tokens) = usage.get("prompt_tokens").and_then(|v| v.as_u64())
            {
                let completion_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_read = usage
                    .get("prompt_tokens_details")
                    .and_then(|v| v.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.usage = if cache_read > 0 {
                    serde_json::json!({
                        "input_tokens": prompt_tokens,
                        "output_tokens": completion_tokens,
                        "cache_read_input_tokens": cache_read
                    })
                } else {
                    serde_json::json!({
                        "input_tokens": prompt_tokens,
                        "output_tokens": completion_tokens
                    })
                };
            }

            let choices = chunk.get("choices").and_then(|v| v.as_array());
            let choice = match choices.and_then(|c| c.first()) {
                Some(c) => c,
                None => return,
            };
            if let Some(delta) = choice.get("delta").and_then(|v| v.as_object()) {
                if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                    let event = serde_json::json!({
                        "type": EVENT_CONTENT_BLOCK_DELTA,
                        "index": self.text_block_index,
                        "delta": { "type": DELTA_TEXT, "text": content }
                    });
                    pending.push_back(Bytes::from(format!(
                        "event: {EVENT_CONTENT_BLOCK_DELTA}\ndata: {}\n\n",
                        event
                    )));
                }

                if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tool_delta in tool_calls {
                        let index = tool_delta
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let entry =
                            self.current_tool_calls
                                .entry(index)
                                .or_insert_with(|| ToolCallState {
                                    id: None,
                                    name: None,
                                    args_buffer: String::new(),
                                    claude_index: None,
                                    started: false,
                                });

                        if let Some(id) = tool_delta.get("id").and_then(|v| v.as_str()) {
                            entry.id = Some(id.to_string());
                        }

                        if let Some(function) =
                            tool_delta.get(TOOL_FUNCTION).and_then(|v| v.as_object())
                        {
                            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                                entry.name = Some(name.to_string());
                            }

                            if !entry.started
                                && let (Some(id), Some(name)) =
                                    (entry.id.clone(), entry.name.clone())
                            {
                                self.tool_block_counter += 1;
                                let claude_index = self.text_block_index + self.tool_block_counter;
                                entry.claude_index = Some(claude_index);
                                entry.started = true;
                                let event = serde_json::json!({
                                    "type": EVENT_CONTENT_BLOCK_START,
                                    "index": claude_index,
                                    "content_block": {
                                        "type": CONTENT_TOOL_USE,
                                        "id": id,
                                        "name": name,
                                        "input": {},
                                        "signature": "",
                                    }
                                });
                                pending.push_back(Bytes::from(format!(
                                    "event: {EVENT_CONTENT_BLOCK_START}\ndata: {}\n\n",
                                    event
                                )));
                            }

                            if entry.started
                                && let Some(arguments) =
                                    function.get("arguments").and_then(|v| v.as_str())
                                && !arguments.is_empty()
                            {
                                entry.args_buffer.push_str(arguments);
                                if let Some(index) = entry.claude_index {
                                    let event = serde_json::json!({
                                        "type": EVENT_CONTENT_BLOCK_DELTA,
                                        "index": index,
                                        "delta": { "type": DELTA_INPUT_JSON, "partial_json": arguments }
                                    });
                                    pending.push_back(Bytes::from(format!(
                                        "event: {EVENT_CONTENT_BLOCK_DELTA}\ndata: {}\n\n",
                                        event
                                    )));
                                }
                            }
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                self.final_stop_reason = match reason {
                    "length" => STOP_MAX_TOKENS,
                    "tool_calls" | "function_call" => STOP_TOOL_USE,
                    _ => STOP_END_TURN,
                };
            }
        }
    }

    fn finish(&mut self, pending: &mut VecDeque<Bytes>) {
        if self.finished {
            return;
        }
        let stop_event = serde_json::json!({
            "type": EVENT_CONTENT_BLOCK_STOP,
            "index": self.text_block_index
        });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_CONTENT_BLOCK_STOP}\ndata: {}\n\n",
            stop_event
        )));

        for tool in self.current_tool_calls.values() {
            if tool.started
                && let Some(index) = tool.claude_index
            {
                let tool_stop = serde_json::json!({
                    "type": EVENT_CONTENT_BLOCK_STOP,
                    "index": index
                });
                pending.push_back(Bytes::from(format!(
                    "event: {EVENT_CONTENT_BLOCK_STOP}\ndata: {}\n\n",
                    tool_stop
                )));
            }
        }

        let message_delta = serde_json::json!({
            "type": EVENT_MESSAGE_DELTA,
            "delta": { "stop_reason": self.final_stop_reason, "stop_sequence": Value::Null },
            "usage": self.usage
        });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_MESSAGE_DELTA}\ndata: {}\n\n",
            message_delta
        )));
        let message_stop = serde_json::json!({ "type": EVENT_MESSAGE_STOP });
        pending.push_back(Bytes::from(format!(
            "event: {EVENT_MESSAGE_STOP}\ndata: {}\n\n",
            message_stop
        )));

        self.finished = true;
    }
}

/// Merge mixed tool_result + text content blocks in user messages.
/// When a user message contains both tool_result and text blocks,
/// text blocks are merged into the preceding tool_result's content
/// to prevent the message from being treated as a fresh user turn.
/// Returns the modified body if any merging was performed.
pub fn merge_tool_result_blocks(body: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let messages = value.get_mut("messages")?.as_array_mut()?;
    let mut modified = false;

    for msg in messages.iter_mut() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" {
            continue;
        }
        let blocks = match msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(b) => b,
            None => continue,
        };

        let has_tool_result = blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some(CONTENT_TOOL_RESULT));
        let has_text = blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some(CONTENT_TEXT));
        if !has_tool_result || !has_text {
            continue;
        }

        let old_blocks = std::mem::take(blocks);
        let mut last_tool_result_idx: Option<usize> = None;

        for block in old_blocks {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if block_type == CONTENT_TOOL_RESULT {
                blocks.push(block);
                last_tool_result_idx = Some(blocks.len() - 1);
            } else if block_type == CONTENT_TEXT && last_tool_result_idx.is_some() {
                let idx = last_tool_result_idx.unwrap();
                let tool_result = &mut blocks[idx];

                let content = tool_result.get_mut("content");
                match content {
                    Some(c) if c.is_array() => {
                        c.as_array_mut().unwrap().push(block);
                    }
                    Some(c) if c.is_string() => {
                        let existing_text = c.as_str().unwrap().to_string();
                        *c = serde_json::json!([
                            {"type": "text", "text": existing_text},
                            block
                        ]);
                    }
                    _ => {
                        tool_result["content"] = serde_json::json!([block]);
                    }
                }
                modified = true;
            } else {
                blocks.push(block);
            }
        }
    }

    if modified {
        serde_json::to_vec(&value).ok()
    } else {
        None
    }
}

fn make_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
