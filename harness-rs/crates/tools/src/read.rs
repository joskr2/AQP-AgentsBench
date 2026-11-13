use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{paths, truncate_output, Tool, ToolContext, ToolError};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Reads a file as numbered lines.
///
/// Arguments:
/// * `path` (required): file path relative to the workspace.
/// * `offset` (optional, 1-based): first line to return. Defaults to 1.
/// * `limit` (optional): number of lines to return. Defaults to 2000.
#[derive(Debug, Default)]
pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file from the workspace and return numbered lines."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the workspace root."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based first line to return."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let raw_path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("read requires a `path` string".to_string()))?;

        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2_000)
            .clamp(1, 10_000) as usize;

        let path = paths::resolve_existing_file(ctx, raw_path)?;

        let metadata = std::fs::metadata(&path)?;
        if metadata.len() > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "file is larger than {} bytes; use `offset`/`limit` on a smaller file",
                MAX_FILE_BYTES
            )));
        }

        let bytes = std::fs::read(&path)?;
        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.lines().collect();

        let start = offset.saturating_sub(1);
        if start >= lines.len() {
            return Ok(format!(
                "(no lines at offset {offset}; file has {} lines)",
                lines.len()
            ));
        }
        let end = (start + limit).min(lines.len());

        let mut output = String::new();
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>6}\t{line}\n", start + index + 1));
        }
        if end < lines.len() {
            output.push_str(&format!("... {} more lines ...\n", lines.len() - end));
        }

        Ok(truncate_output(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempWorkspace {
        root: std::path::PathBuf,
    }

    impl TempWorkspace {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "dshrs-tools-read-{}-{nanos}-{label}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn root(&self) -> &std::path::Path {
            &self.root
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn reads_a_line_window() {
        let workspace = TempWorkspace::new("window");
        std::fs::write(workspace.root().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let output = ReadTool::new()
            .execute(json!({ "path": "a.txt", "offset": 2, "limit": 1 }), &ctx)
            .await
            .unwrap();

        assert!(output.contains("two"));
        assert!(!output.contains("one"));
        assert!(!output.contains("three"));
    }

    #[tokio::test]
    async fn rejects_escape_and_directories() {
        let workspace = TempWorkspace::new("escape");
        std::fs::create_dir_all(workspace.root().join("dir")).unwrap();
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let escape = ReadTool::new()
            .execute(json!({ "path": "../outside.txt" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(escape, ToolError::Denied(_)));

        let directory = ReadTool::new()
            .execute(json!({ "path": "dir" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(directory, ToolError::InvalidArgs(_)));
    }
}
