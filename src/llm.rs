//! Shared LLM surface handlers reused by `/v1/*`, Amp, and Droid routes.

use crate::claude::{
    analyze_claude_request, convert_claude_request, convert_openai_response, error_from_proxy,
    extract_anthropic_model, is_native_claude_model, merge_tool_result_blocks,
    validate_anthropic_headers,
};
use crate::error::Error;
use crate::gemini::{convert_gemini_request, convert_openai_to_gemini_response};
use crate::initiator::{
    RequestAnalysis, analyze_openai_chat_completions, analyze_openai_responses,
};
use crate::proxy::forward_response;
use crate::server::AppState;
use crate::token_counter::handle_count_tokens;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use serde_json::Value;

fn analyze_openai_request(
    path: &str,
    method: &Method,
    body: &[u8],
    headers: &HeaderMap,
) -> Option<RequestAnalysis> {
    if *method != Method::POST {
        return None;
    }
    match path {
        "chat/completions" => Some(analyze_openai_chat_completions(body, Some(headers))),
        "responses" => Some(analyze_openai_responses(body, Some(headers))),
        _ => None,
    }
}

pub async fn handle_openai_passthrough(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let analysis = analyze_openai_request(api_path, &method, &body, headers);

    let resp = state
        .proxy
        .forward(
            &format!("/{}{}", api_path, query),
            method,
            body,
            content_type,
            analysis.map(|a| a.initiator),
            analysis.map(|a| a.is_vision).unwrap_or(false),
        )
        .await?;
    forward_response(resp).await
}

pub async fn handle_anthropic_compat(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
    validate_client_api_key: bool,
) -> Result<Response, Error> {
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());

    match api_path {
        "messages/count_tokens" => {
            if method != Method::POST {
                return Ok(error_from_proxy(Error::InvalidRequest(
                    "Only POST is supported for messages/count_tokens".to_string(),
                )));
            }
            if let Some(model) = extract_anthropic_model(&body)
                && is_native_claude_model(&model)
            {
                let resp = match state
                    .proxy
                    .forward(
                        &format!("/v1/messages/count_tokens{query}"),
                        method,
                        body,
                        content_type,
                        None,
                        false,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => return Ok(error_from_proxy(err)),
                };
                return forward_response(resp).await;
            }
            handle_count_tokens(body).await
        }
        "messages" => {
            if method != Method::POST {
                return Ok(error_from_proxy(Error::InvalidRequest(
                    "Only POST is supported for messages".to_string(),
                )));
            }

            if validate_client_api_key && let Some(resp) = validate_anthropic_headers(headers) {
                return Ok(resp);
            }

            let metadata = match analyze_claude_request(&body, Some(headers)) {
                Ok(metadata) => metadata,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            if is_native_claude_model(&metadata.model) {
                let forwarded_body = merge_tool_result_blocks(&body)
                    .map(Bytes::from)
                    .unwrap_or(body);
                let resp = match state
                    .proxy
                    .forward(
                        &format!("/v1/messages{}", query),
                        method,
                        forwarded_body,
                        content_type,
                        Some(&metadata.initiator),
                        metadata.is_vision,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => return Ok(error_from_proxy(err)),
                };
                return forward_response(resp).await;
            }

            let converted = match convert_claude_request(body, Some(headers)) {
                Ok(converted) => converted,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            let resp = match state
                .proxy
                .forward(
                    &format!("/chat/completions{}", query),
                    method,
                    converted.body,
                    Some("application/json"),
                    Some(&converted.initiator),
                    converted.is_vision,
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            match convert_openai_response(resp, converted.model, converted.stream).await {
                Ok(response) => Ok(response),
                Err(err) => Ok(error_from_proxy(err)),
            }
        }
        _ => {
            handle_openai_passthrough(
                state,
                method,
                api_path,
                Some(query.trim_start_matches('?')),
                headers,
                body,
            )
            .await
        }
    }
}

pub async fn handle_gemini_generate_content(
    state: &AppState,
    method: Method,
    model: &str,
    query: Option<&str>,
    body: Bytes,
    stream: bool,
) -> Result<Response, Error> {
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let converted = match convert_gemini_request(model, body, stream) {
        Ok(c) => c,
        Err(e) => return Ok(error_from_proxy(e)),
    };
    let resp = match state
        .proxy
        .forward(
            &format!("/chat/completions{}", query),
            method,
            converted.body,
            Some("application/json"),
            Some(converted.initiator),
            converted.is_vision,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(error_from_proxy(e)),
    };
    match convert_openai_to_gemini_response(resp, converted.model, converted.stream).await {
        Ok(r) => Ok(r),
        Err(e) => Ok(error_from_proxy(e)),
    }
}

pub fn extract_model_field(body: &[u8]) -> Result<String, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;
    value
        .get("model")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidRequest("Missing required field: model".to_string()))
}
