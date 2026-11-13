use async_trait::async_trait;
use futures_util::stream::unfold;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{
    DeltaStream, LlmDelta, LlmError, LlmProvider, LlmRequest, Message, Result, ToolCall, ToolSchema,
};

const MAX_ERROR_BODY: usize = 500;

/// Minimal client for any OpenAI-compatible `/chat/completions` endpoint.
///
/// Handles server-sent events by hand: chunks are buffered as bytes, split on
/// `\n`, and each `data:` payload is parsed as JSON. Streaming tool-call
/// fragments are re-assembled here so the agent loop receives whole calls.
pub struct OpenAiCompatible {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            client,
        })
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn request_body(&self, request: &LlmRequest) -> Value {
        let messages: Vec<Value> = request.messages.iter().map(message_to_wire).collect();
        let mut body = json!({
            "model": self.model.clone(),
            "messages": messages,
            "stream": true,
        });
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request.tools.iter().map(tool_to_wire).collect();
            body["tools"] = Value::Array(tools);
        }
        body
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatible {
    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, request: LlmRequest) -> Result<DeltaStream> {
        let mut builder = self
            .client
            .post(self.chat_completions_url())
            .json(&self.request_body(&request));
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }

        tracing::debug!(url = %self.chat_completions_url(), "sending chat completion request");
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(LlmError::Provider(format!(
                "http {status}: {}",
                truncate(&body, MAX_ERROR_BODY)
            )));
        }

        let (sender, receiver) = mpsc::channel::<Result<LlmDelta>>(64);
        tokio::spawn(parse_sse(response, sender));

        let stream = unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        Ok(Box::pin(stream))
    }
}

fn message_to_wire(message: &Message) -> Value {
    let mut wire = json!({ "role": message.role });
    // OpenAI omits `content` for pure tool-call assistant messages.
    if !message.content.is_empty() || message.tool_calls.is_empty() {
        wire["content"] = Value::String(message.content.clone());
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id.clone(),
                    "type": "function",
                    "function": {
                        "name": call.name.clone(),
                        "arguments": call.arguments.to_string(),
                    },
                })
            })
            .collect();
        wire["tool_calls"] = Value::Array(calls);
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        wire["tool_call_id"] = Value::String(tool_call_id.clone());
    }
    wire
}

fn tool_to_wire(schema: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": schema.name.clone(),
            "description": schema.description.clone(),
            "parameters": schema.parameters.clone(),
        },
    })
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

async fn parse_sse(response: reqwest::Response, sender: mpsc::Sender<Result<LlmDelta>>) {
    let mut chunks = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut pending_calls: Vec<PendingToolCall> = Vec::new();

    'read: loop {
        let chunk = match chunks.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                let _ = sender.send(Err(LlmError::Http(error))).await;
                return;
            }
            None => break,
        };
        buffer.extend_from_slice(&chunk);

        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=position).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(_) => {
                    let _ = sender
                        .send(Err(LlmError::Protocol(
                            "sse line was not valid utf-8".to_string(),
                        )))
                        .await;
                    return;
                }
            };
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            match parse_data_line(data.trim(), &mut pending_calls, &sender).await {
                Ok(true) => break 'read,
                Ok(false) => {}
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            }
        }
    }

    // Be lenient with servers that omit the final newline.
    if !buffer.is_empty() {
        let mut line = String::from_utf8_lossy(&buffer).to_string();
        if line.ends_with('\r') {
            line.pop();
        }
        if let Some(data) = line.strip_prefix("data:") {
            if let Err(error) = parse_data_line(data.trim(), &mut pending_calls, &sender).await {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }

    for (index, pending) in pending_calls.into_iter().enumerate() {
        let id = if pending.id.is_empty() {
            format!("call_{index}")
        } else {
            pending.id
        };
        let arguments = if pending.arguments.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&pending.arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    let _ = sender
                        .send(Err(LlmError::Protocol(format!(
                            "invalid arguments for tool {}: {error}",
                            pending.name
                        ))))
                        .await;
                    return;
                }
            }
        };
        let tool_call = ToolCall {
            id,
            name: pending.name,
            arguments,
        };
        if sender
            .send(Ok(LlmDelta::ToolCall(tool_call)))
            .await
            .is_err()
        {
            return;
        }
    }

    let _ = sender.send(Ok(LlmDelta::Finished)).await;
}

/// Handles one SSE `data:` payload. Returns `true` when the payload is `[DONE]`.
async fn parse_data_line(
    data: &str,
    pending_calls: &mut Vec<PendingToolCall>,
    sender: &mpsc::Sender<Result<LlmDelta>>,
) -> Result<bool> {
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }

    let payload: Value = serde_json::from_str(data)
        .map_err(|error| LlmError::Protocol(format!("invalid sse json: {error}")))?;

    if let Some(error) = payload.get("error") {
        return Err(LlmError::Provider(error.to_string()));
    }

    let delta = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"));
    let Some(delta) = delta else {
        return Ok(false);
    };

    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        if !text.is_empty()
            && sender
                .send(Ok(LlmDelta::Text(text.to_string())))
                .await
                .is_err()
        {
            return Err(LlmError::Provider(
                "consumer dropped the delta stream".to_string(),
            ));
        }
    }

    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(pending_calls.len() as u64) as usize;
            while pending_calls.len() <= index {
                pending_calls.push(PendingToolCall::default());
            }
            let pending = &mut pending_calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                pending.id.push_str(id);
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    pending.name.push_str(name);
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    pending.arguments.push_str(arguments);
                }
            }
        }
    }

    Ok(false)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn wire_messages_keep_tool_calls_and_results() {
        let provider =
            OpenAiCompatible::new("http://localhost:1234/v1/", "test-model", None).unwrap();
        let request = LlmRequest {
            messages: vec![
                Message::assistant_tool_calls(vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "path": "a.txt" }),
                }]),
                Message::tool("call-1", "contents"),
            ],
            tools: vec![ToolSchema {
                name: "read".to_string(),
                description: "reads a file".to_string(),
                parameters: json!({ "type": "object" }),
            }],
        };

        let body = provider.request_body(&request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["name"],
            "read"
        );
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "call-1");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        // Multi-byte character cut must not panic nor split a code point.
        let truncated = truncate("aaaaaaaaé", 9);
        assert!(truncated.ends_with('…'));
    }
}
