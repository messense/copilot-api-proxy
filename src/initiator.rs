//! Initiator inference for sticky inference cost savings.
//!
//! This module implements "sticky inference": once a conversation has assistant/tool
//! messages, all subsequent requests are marked as agent-initiated and do not
//! consume Copilot premium requests.

use serde_json::Value;

/// Result of analyzing a request body for initiator and vision detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAnalysis {
    pub initiator: &'static str,
    pub is_vision: bool,
}

/// Infer the initiator from Claude/Anthropic format message history.
/// Returns "agent" if any assistant/tool messages exist, "user" otherwise.
pub fn infer_initiator_claude(messages: &[Value]) -> &'static str {
    infer_initiator_from_messages(messages, &["assistant", "tool"])
}

/// Analyze OpenAI chat completions request for initiator and vision.
pub fn analyze_openai_chat_completions(body: &[u8]) -> RequestAnalysis {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let Some(messages) = value.get("messages").and_then(|v| v.as_array()) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let initiator = infer_initiator_from_messages(messages, &["assistant", "tool"]);
    let is_vision = messages.iter().any(|msg| {
        msg.get("content")
            .and_then(|c| c.as_array())
            .map(|parts| parts.iter().any(|p| p.get("type") == Some(&Value::String("image_url".to_string()))))
            .unwrap_or(false)
    });
    RequestAnalysis { initiator, is_vision }
}

/// Analyze OpenAI responses API request for initiator and vision.
pub fn analyze_openai_responses(body: &[u8]) -> RequestAnalysis {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let Some(input) = value.get("input") else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    if input.is_string() {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    }
    let Some(items) = input.as_array() else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let initiator = infer_initiator_from_messages(items, &["assistant", "tool"]);
    let is_vision = items.iter().any(|item| {
        item.get("content")
            .and_then(|c| c.as_array())
            .map(|parts| parts.iter().any(|p| p.get("type") == Some(&Value::String("input_image".to_string()))))
            .unwrap_or(false)
    });
    RequestAnalysis { initiator, is_vision }
}

fn infer_initiator_from_messages(messages: &[Value], agent_roles: &[&str]) -> &'static str {
    if messages.iter().any(|msg| {
        msg.get("role")
            .and_then(|v| v.as_str())
            .map(|r| agent_roles.contains(&r))
            .unwrap_or(false)
    }) {
        "agent"
    } else {
        "user"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_claude_user_only() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        assert_eq!(infer_initiator_claude(&messages), "user");
    }

    #[test]
    fn test_claude_with_assistant() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ];
        assert_eq!(infer_initiator_claude(&messages), "agent");
    }

    #[test]
    fn test_claude_with_tool() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "tool", "content": "result"}),
        ];
        assert_eq!(infer_initiator_claude(&messages), "agent");
    }

    #[test]
    fn test_claude_multi_turn() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi"}),
            json!({"role": "user", "content": "How are you?"}),
        ];
        assert_eq!(infer_initiator_claude(&messages), "agent");
    }

    #[test]
    fn test_claude_empty() {
        let messages: Vec<Value> = vec![];
        assert_eq!(infer_initiator_claude(&messages), "user");
    }

    #[test]
    fn test_openai_user_only() {
        let body = json!({"messages": [{"role": "user", "content": "Hello"}]});
        let result = analyze_openai_chat_completions(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_assistant() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes());
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_tool() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "tool", "tool_call_id": "123", "content": "result"}
            ]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes());
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_invalid_json() {
        let result = analyze_openai_chat_completions(b"not json");
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_missing_messages() {
        let body = json!({"model": "gpt-4"});
        let result = analyze_openai_chat_completions(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_vision() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
                ]
            }]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(result.is_vision);
    }

    #[test]
    fn test_responses_string_input() {
        let body = json!({"input": "Hello, world!"});
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_user_only() {
        let body = json!({
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Hello"}]}]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_assistant() {
        let body = json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]}
            ]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_tool() {
        let body = json!({
            "input": [
                {"role": "user", "content": "Hello"},
                {"role": "tool", "content": "result"}
            ]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_invalid_json() {
        let result = analyze_openai_responses(b"not json");
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_missing_input() {
        let body = json!({"model": "gpt-4"});
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_vision() {
        let body = json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What's in this image?"},
                    {"type": "input_image", "image_url": "https://example.com/image.png"}
                ]
            }]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes());
        assert_eq!(result.initiator, "user");
        assert!(result.is_vision);
    }
}
