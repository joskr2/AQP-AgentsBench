use std::io::Write;
use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{paths, Tool, ToolContext, ToolError};

/// Same bound the read tool applies; editing a huge file is never the intent.
const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024;

/// Replaces exactly one literal occurrence and writes atomically.
///
/// Ambiguity is an error: if `old_string` appears zero or multiple times the
/// edit is rejected so the model must provide more context. The new content is
/// written to a temporary file next to the target and renamed over it, so a
/// crash cannot leave a half-written file.
///
/// Note: `rename` over an existing file is atomic on Unix-like systems; on
/// Windows it fails, so this skeleton targets Unix (macOS/Linux).
///
/// Arguments:
/// * `path` (required): file path relative to the workspace.
/// * `old_string` (required): literal text to replace, must be unique.
/// * `new_string` (required): replacement text.
#[derive(Debug, Default)]
pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace one unique literal string in a workspace file (atomic write)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the workspace root."
                },
                "old_string": {
                    "type": "string",
                    "description": "Literal text to replace; must occur exactly once."
                },
                "new_string": {
                    "type": "string",
                    "description": "Literal replacement text."
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let raw_path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("edit requires a `path` string".to_string()))?;
        let old_string = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("edit requires `old_string`".to_string()))?;
        let new_string = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("edit requires `new_string`".to_string()))?;

        if old_string.is_empty() {
            return Err(ToolError::InvalidArgs(
                "`old_string` must not be empty".to_string(),
            ));
        }
        if old_string == new_string {
            return Err(ToolError::InvalidArgs(
                "`old_string` and `new_string` are identical".to_string(),
            ));
        }

        let path = paths::resolve_existing_file(ctx, raw_path)?;
        let size = std::fs::metadata(&path)?.len();
        if size > MAX_EDIT_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "file is too large to edit ({size} bytes, limit {MAX_EDIT_BYTES})"
            )));
        }
        let content = std::fs::read_to_string(&path)?;

        let occurrences = content.match_indices(old_string).count();
        if occurrences == 0 {
            return Err(ToolError::Failed(format!(
                "`old_string` was not found in {raw_path}"
            )));
        }
        if occurrences > 1 {
            return Err(ToolError::Failed(format!(
                "`old_string` appears {occurrences} times in {raw_path}; add surrounding context so it is unique"
            )));
        }

        let updated = content.replacen(old_string, new_string, 1);
        atomic_write(&path, updated.as_bytes())?;

        Ok(format!(
            "edited {raw_path} ({} bytes -> {} bytes)",
            content.len(),
            updated.len()
        ))
    }
}

fn atomic_write(target: &Path, contents: &[u8]) -> Result<(), ToolError> {
    let parent = target.parent().ok_or_else(|| {
        ToolError::Failed(format!(
            "file has no parent directory: {}",
            target.display()
        ))
    })?;

    // A rename replaces the inode, so a fresh temporary file would silently
    // reset an executable or read-only file to the default mode. Keep the
    // original permissions when the target already exists.
    let permissions = std::fs::metadata(target)
        .map(|metadata| metadata.permissions())
        .ok();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(".harn-edit-{}-{nanos}.tmp", std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        if let Some(permissions) = &permissions {
            std::fs::set_permissions(&temporary, permissions.clone())?;
        }
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(ToolError::Io(error));
    }

    if let Err(error) = std::fs::rename(&temporary, target) {
        let _ = std::fs::remove_file(&temporary);
        return Err(ToolError::Io(error));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "dshrs-tools-edit-{}-{nanos}-{unique}-{label}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn replaces_a_single_occurrence() {
        let workspace = TempWorkspace::new("ok");
        let path = workspace.root().join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let output = EditTool::new()
            .execute(
                json!({ "path": "notes.txt", "old_string": "beta", "new_string": "BETA" }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.contains("notes.txt"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_and_missing_old_string() {
        let workspace = TempWorkspace::new("ambiguous");
        let path = workspace.root().join("notes.txt");
        std::fs::write(&path, "same\nother\nsame\n").unwrap();
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let ambiguous = EditTool::new()
            .execute(
                json!({ "path": "notes.txt", "old_string": "same", "new_string": "x" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(ambiguous, ToolError::Failed(_)));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "same\nother\nsame\n"
        );

        let missing = EditTool::new()
            .execute(
                json!({ "path": "notes.txt", "old_string": "absent", "new_string": "x" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn rejects_parent_and_absolute_paths() {
        let workspace = TempWorkspace::new("lexical");
        let ctx = ToolContext::new(workspace.root()).unwrap();

        let parent = EditTool::new()
            .execute(
                json!({ "path": "../escape.txt", "old_string": "a", "new_string": "b" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(parent, ToolError::Denied(_)));

        let absolute = EditTool::new()
            .execute(
                json!({ "path": "/etc/passwd", "old_string": "a", "new_string": "b" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(absolute, ToolError::Denied(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_that_escapes_the_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = TempWorkspace::new("symlink");
        let outside = TempWorkspace::new("symlink-outside");
        let outside_file = outside.root().join("secret.txt");
        std::fs::write(&outside_file, "secret\n").unwrap();
        symlink(&outside_file, workspace.root().join("link.txt")).unwrap();

        let ctx = ToolContext::new(workspace.root()).unwrap();
        let error = EditTool::new()
            .execute(
                json!({ "path": "link.txt", "old_string": "secret", "new_string": "pwned" }),
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::Denied(_)));
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "secret\n");
    }

    #[test]
    fn atomic_write_replaces_file_contents() {
        let workspace = TempWorkspace::new("atomic");
        let path = workspace.root().join("file.txt");
        std::fs::write(&path, "old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // No temporary files are left behind.
        let leftovers = std::fs::read_dir(workspace.root())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".harn-edit-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_the_original_mode() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempWorkspace::new("mode");
        let path = workspace.root().join("run.sh");
        std::fs::write(&path, "#!/bin/sh\necho old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&path, b"#!/bin/sh\necho new\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho new\n"
        );
    }
}
