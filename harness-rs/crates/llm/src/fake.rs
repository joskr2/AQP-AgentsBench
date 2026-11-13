use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::stream;

use crate::{DeltaStream, LlmDelta, LlmError, LlmProvider, LlmRequest, Result, ToolCall};

/// One scripted provider response. Turns are consumed in order, one per step.
#[derive(Debug, Clone)]
pub struct FakeTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

impl FakeTurn {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }

    pub fn with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            text: String::new(),
            tool_calls,
        }
    }
}

/// Deterministic provider for tests: replays a fixed script.
#[derive(Debug)]
pub struct FakeProvider {
    model: String,
    script: Mutex<VecDeque<FakeTurn>>,
    calls: AtomicUsize,
}

impl FakeProvider {
    pub fn new(model: impl Into<String>, script: Vec<FakeTurn>) -> Self {
        Self {
            model: model.into(),
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
        }
    }

    /// Number of completed requests, useful for asserting `max_steps`.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for FakeProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, _request: LlmRequest) -> Result<DeltaStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        let turn = self
            .script
            .lock()
            .map_err(|_| LlmError::Provider("fake provider script poisoned".to_string()))?
            .pop_front();

        let Some(turn) = turn else {
            return Err(LlmError::Provider(
                "fake provider script exhausted: add another FakeTurn".to_string(),
            ));
        };

        let mut deltas: Vec<Result<LlmDelta>> = Vec::with_capacity(turn.tool_calls.len() + 2);
        if !turn.text.is_empty() {
            deltas.push(Ok(LlmDelta::Text(turn.text)));
        }
        for tool_call in turn.tool_calls {
            deltas.push(Ok(LlmDelta::ToolCall(tool_call)));
        }
        deltas.push(Ok(LlmDelta::Finished));

        Ok(Box::pin(stream::iter(deltas)))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::{LlmDelta, LlmProvider, LlmRequest};

    #[tokio::test]
    async fn streams_scripted_turns_in_order() {
        let provider = FakeProvider::new(
            "fake",
            vec![
                FakeTurn::text("hello"),
                FakeTurn::with_tool_calls(vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }]),
            ],
        );

        let request = || LlmRequest::default();

        let stream = provider.stream(request()).await.unwrap();
        let deltas: Vec<LlmDelta> = stream.map(|item| item.unwrap()).collect().await;
        assert_eq!(
            deltas,
            vec![LlmDelta::Text("hello".to_string()), LlmDelta::Finished]
        );

        let stream = provider.stream(request()).await.unwrap();
        let deltas: Vec<LlmDelta> = stream.map(|item| item.unwrap()).collect().await;
        assert_eq!(deltas.len(), 2);
        assert_eq!(
            deltas[0],
            LlmDelta::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            })
        );
        assert_eq!(deltas[1], LlmDelta::Finished);
        assert_eq!(provider.calls(), 2);

        assert!(provider.stream(request()).await.is_err());
    }
}
