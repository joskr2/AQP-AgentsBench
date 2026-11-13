//! Tool seam for the harness.
//!
//! Every tool is `Send + Sync` and receives a [`ToolContext`]. All path-like
//! arguments are confined to the workspace root: absolute paths, `..` segments,
//! and symlinks that resolve outside the root are rejected.
//!
//! The three shipped tools are deliberately small seams, not a shell:
//! * [`BashTool`] - argv-only process execution with a timeout.
//! * [`ReadTool`] - line-oriented file reads.
//! * [`EditTool`] - unique literal replacement written atomically.
#![forbid(unsafe_code)]

mod bash;
mod edit;
mod paths;
mod read;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

/// Hard cap for tool output returned to the model.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArgs(String),
    #[error("access denied: {0}")]
    Denied(String),
    #[error("tool io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool timed out after {0:?}")]
    Timeout(Duration),
    #[error("tool failed: {0}")]
    Failed(String),
}

/// Execution context shared by every tool.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub(crate) workspace: PathBuf,
    pub(crate) default_timeout: Duration,
}

impl ToolContext {
    /// Canonicalizes the workspace root once; later path checks are plain
    /// prefix comparisons against this canonical path.
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let workspace = std::fs::canonicalize(workspace.into())?;
        Ok(Self {
            workspace,
            default_timeout: Duration::from_secs(30),
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }
}

/// One executable capability exposed to the model.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments object.
    fn schema(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
}

/// Name-keyed bag of tools. The agent turns this into provider tool schemas.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with the three built-in tools.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(BashTool::new()));
        registry.register(Arc::new(ReadTool::new()));
        registry.register(Arc::new(EditTool::new()));
        registry
    }

    /// Registers a tool, replacing any previous tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.values()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(|name| name.as_str())
    }

    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let tool = self.tools.get(name).ok_or_else(|| {
            ToolError::Failed(format!(
                "unknown tool `{name}`; available: {}",
                self.names().collect::<Vec<_>>().join(", ")
            ))
        })?;
        tool.execute(args, ctx).await
    }
}

pub(crate) fn truncate_output(text: String) -> String {
    if text.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return text;
    }
    let mut truncated: String = text.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
    truncated.push_str("\n[output truncated]");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_tool_is_a_tool_error() {
        let registry = ToolRegistry::new();
        let ctx = ToolContext::new(std::env::temp_dir()).unwrap();
        let error = registry
            .execute("nope", Value::Null, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed(_)));
    }

    #[test]
    fn truncate_output_marks_long_output() {
        let long = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 10);
        let truncated = truncate_output(long);
        assert!(truncated.ends_with("[output truncated]"));
        assert_eq!(
            truncated.trim_end_matches("[output truncated]").chars().count(),
            MAX_TOOL_OUTPUT_CHARS + 1 // the inserted newline
        );
    }
}
