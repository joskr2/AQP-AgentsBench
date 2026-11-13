use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::{paths, truncate_output, Tool, ToolContext, ToolError};

/// Runs a subprocess from an argv array; no shell is involved.
///
/// Arguments:
/// * `argv` (required): non-empty array of strings.
/// * `cwd` (optional): directory relative to the workspace root.
/// * `timeout_ms` (optional): overrides the context timeout.
#[derive(Debug, Default)]
pub struct BashTool;

impl BashTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a command without a shell. Pass the executable and arguments as `argv`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Executable followed by its arguments; no shell expansion."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory relative to the workspace root."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional timeout for this command in milliseconds."
                }
            },
            "required": ["argv"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let argv = parse_argv(&args)?;

        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(dir) if !dir.is_empty() => paths::resolve_existing_dir(ctx, dir)?,
            _ => ctx.workspace.clone(),
        };

        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(ctx.default_timeout);

        tracing::debug!(?argv, cwd = %cwd.display(), "running bash tool");
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command.spawn().map_err(|error| {
            ToolError::Failed(format!("failed to spawn {:?}: {error}", argv[0]))
        })?;

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| ToolError::Timeout(timeout))?
            .map_err(ToolError::Io)?;

        let mut text = String::new();
        if !output.stdout.is_empty() {
            text.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        let status = match output.status.code() {
            Some(code) => format!("exit={code}"),
            None => "exit=signal".to_string(),
        };
        Ok(truncate_output(format!("{status}\n{text}")))
    }
}

fn parse_argv(args: &Value) -> Result<Vec<String>, ToolError> {
    let values = args.get("argv").and_then(Value::as_array).ok_or_else(|| {
        ToolError::InvalidArgs("bash requires an `argv` array of strings".to_string())
    })?;

    let mut argv = Vec::with_capacity(values.len());
    for value in values {
        let argument = value.as_str().ok_or_else(|| {
            ToolError::InvalidArgs("every `argv` entry must be a string".to_string())
        })?;
        argv.push(argument.to_string());
    }

    if argv.is_empty() {
        return Err(ToolError::InvalidArgs(
            "`argv` must not be empty".to_string(),
        ));
    }
    Ok(argv)
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
                "dshrs-tools-bash-{}-{nanos}-{label}",
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

    #[test]
    fn rejects_non_string_or_empty_argv() {
        assert!(matches!(
            parse_argv(&json!({ "argv": [] })),
            Err(ToolError::InvalidArgs(_))
        ));
        assert!(matches!(
            parse_argv(&json!({ "argv": ["ls", 1] })),
            Err(ToolError::InvalidArgs(_))
        ));
        assert!(parse_argv(&json!({ "argv": ["ls", "-la"] })).is_ok());
    }

    #[tokio::test]
    async fn runs_a_command_without_a_shell() {
        let workspace = TempWorkspace::new("run");
        std::fs::write(workspace.root().join("hello.txt"), "hello\n").unwrap();
        let ctx = ToolContext::new(workspace.root()).unwrap();

        // `echo` receives literal `$HOME`; a shell would expand it.
        let output = BashTool::new()
            .execute(json!({ "argv": ["echo", "$HOME"] }), &ctx)
            .await
            .unwrap();

        assert!(output.starts_with("exit=0"));
        assert!(output.contains("$HOME"));
    }

    #[tokio::test]
    async fn rejects_cwd_outside_the_workspace() {
        let workspace = TempWorkspace::new("cwd");
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let error = BashTool::new()
            .execute(json!({ "argv": ["pwd"], "cwd": "../" }), &ctx)
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::Denied(_)));
    }
}
