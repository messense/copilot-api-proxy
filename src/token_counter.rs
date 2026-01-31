//! Token counting for Anthropic /v1/messages/count_tokens endpoint.

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

use crate::claude::convert_claude_request;
use crate::error::Error;

/// Handle /v1/messages/count_tokens endpoint.
/// Returns {"input_tokens": N} or {"input_tokens": 1} on error.
pub async fn handle_count_tokens(body: Bytes) -> Result<Response, Error> {
    let token_count = match count_tokens_internal(&body).await {
        Ok(count) => count,
        Err(e) => {
            tracing::warn!("Token counting failed: {}, returning default", e);
            1
        }
    };

    let response_body = serde_json::json!({
        "input_tokens": token_count
    });

    let json = serde_json::to_vec(&response_body)
        .map_err(|e| Error::InvalidRequest(format!("Failed to serialize response: {e}")))?;

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(json))
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

async fn count_tokens_internal(body: &[u8]) -> Result<u64, Error> {
    // Parse the Anthropic request
    let anthropic_value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON: {e}")))?;

    // Get model name for multiplier
    let model = anthropic_value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Convert to OpenAI format (reuse existing logic)
    let converted = convert_claude_request(Bytes::copy_from_slice(body))?;
    let openai_value: Value = serde_json::from_slice(&converted.body)
        .map_err(|e| Error::InvalidRequest(format!("Failed to parse converted request: {e}")))?;

    // Count tokens using tiktoken
    let mut total_tokens = count_openai_tokens(&openai_value)?;

    // Add tool overhead if tools are present
    if let Some(tools) = anthropic_value.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            // Check if any tool starts with "mcp__" (MCP tools don't count)
            let has_mcp_tools = tools.iter().any(|tool| {
                tool.get("name")
                    .and_then(|v| v.as_str())
                    .map(|name| name.starts_with("mcp__"))
                    .unwrap_or(false)
            });

            if !has_mcp_tools {
                // Add fixed overhead for tools
                // Claude: ~346 tokens, Grok: ~480 tokens
                // Use 400 as a reasonable middle ground
                total_tokens += 400;
            }
        }
    }

    // Apply model-specific multiplier
    let multiplier = if model.contains("claude") {
        1.15
    } else if model.contains("grok") {
        1.03
    } else {
        1.0
    };

    let final_count = (total_tokens as f64 * multiplier).round() as u64;
    Ok(final_count)
}

fn count_openai_tokens(openai_value: &Value) -> Result<u64, Error> {
    // Get the tokenizer (o200k_base is used by GPT-4o and newer models)
    let bpe = tiktoken_rs::o200k_base()
        .map_err(|e| Error::InvalidRequest(format!("Failed to load tokenizer: {e}")))?;

    let messages = openai_value
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidRequest("Missing messages field".to_string()))?;

    let mut total = 0u64;

    // Count message tokens
    for message in messages {
        total += count_message_tokens(message, &bpe)?;
    }

    // Count tool tokens if present
    if let Some(tools) = openai_value.get("tools").and_then(|v| v.as_array()) {
        for tool in tools {
            total += count_tool_tokens(tool, &bpe)?;
        }
    }

    // Add base overhead (every reply is primed with tokens)
    total += 3;

    Ok(total)
}

fn count_message_tokens(message: &Value, bpe: &CoreBPE) -> Result<u64, Error> {
    let mut tokens = 3; // Base tokens per message

    // Count role
    if let Some(role) = message.get("role").and_then(|v| v.as_str()) {
        tokens += bpe.encode_with_special_tokens(role).len() as u64;
    }

    // Count content
    if let Some(content) = message.get("content") {
        tokens += count_content_tokens(content, bpe)?;
    }

    // Count name if present
    if message.get("name").is_some() {
        tokens += 1;
    }

    // Count tool_calls if present
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tool_call in tool_calls {
            tokens += count_tool_call_tokens(tool_call, bpe)?;
        }
    }

    Ok(tokens)
}

fn count_content_tokens(content: &Value, bpe: &CoreBPE) -> Result<u64, Error> {
    match content {
        Value::String(text) => Ok(bpe.encode_with_special_tokens(text).len() as u64),
        Value::Array(parts) => {
            let mut tokens = 0u64;
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    tokens += bpe.encode_with_special_tokens(text).len() as u64;
                } else if part.get("type").and_then(|v| v.as_str()) == Some("image_url") {
                    // Image tokens: rough estimate
                    if let Some(url) = part
                        .get("image_url")
                        .and_then(|v| v.get("url"))
                        .and_then(|v| v.as_str())
                    {
                        tokens += bpe.encode_with_special_tokens(url).len() as u64 + 85;
                    }
                }
            }
            Ok(tokens)
        }
        _ => Ok(0),
    }
}

fn count_tool_call_tokens(tool_call: &Value, bpe: &CoreBPE) -> Result<u64, Error> {
    let mut tokens = 7; // Base overhead for tool call

    if let Some(function) = tool_call.get("function").and_then(|v| v.as_object()) {
        if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
            tokens += bpe.encode_with_special_tokens(name).len() as u64;
        }
        if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
            tokens += bpe.encode_with_special_tokens(arguments).len() as u64;
        }
    }

    tokens += 12; // End overhead
    Ok(tokens)
}

fn count_tool_tokens(tool: &Value, bpe: &CoreBPE) -> Result<u64, Error> {
    let mut tokens = 7; // Base overhead for tool definition

    if let Some(function) = tool.get("function").and_then(|v| v.as_object()) {
        if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
            tokens += bpe.encode_with_special_tokens(name).len() as u64;
        }
        if let Some(description) = function.get("description").and_then(|v| v.as_str()) {
            tokens += bpe.encode_with_special_tokens(description).len() as u64;
        }
        if let Some(parameters) = function.get("parameters") {
            // Rough estimate: serialize and count
            let params_str = serde_json::to_string(parameters).unwrap_or_default();
            tokens += bpe.encode_with_special_tokens(&params_str).len() as u64;
        }
    }

    tokens += 12; // End overhead
    Ok(tokens)
}
