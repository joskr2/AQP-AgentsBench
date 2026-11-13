//! Provider seam for the harness.
//!
//! The agent loop only knows about [`LlmProvider`], [`Message`], and streaming
//! [`LlmDelta`]s. Two implementations ship here: a deterministic
//! [`FakeProvider`] for tests and an [`OpenAiCompatible`] client that parses
//! SSE by hand (no extra SSE dependency).
#![forbid(unsafe_code)]

mod fake;
mod openai;

pub use fake::{FakeProvider, FakeTurn};
pub use openai::OpenAiCompatible;

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, LlmError>;

/// Everything that can go wrong at the provider seam.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid provider response: {0}")]
    Protocol(String),
    #[error("provider error: {0}")]
    Provider(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A protocol-neutral chat message.
///
/// `tool_calls` is only populated for assistant messages that requested tools;
/// `tool_call_id` is only populated for tool-result messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// JSON Schema description of a tool, sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
}

/// One streamed chunk produced by a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmDelta {
    Text(String),
    ToolCall(ToolCall),
    Finished,
}

pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<LlmDelta>> + Send>>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model(&self) -> &str;

    /// Starts a streaming completion. Tool calls are yielded fully assembled;
    /// partial JSON argument fragments never escape the provider.
    async fn stream(&self, request: LlmRequest) -> Result<DeltaStream>;
}
