use std::path::{Component, Path, PathBuf};

use crate::{ToolContext, ToolError};

/// Resolves a tool-supplied path inside the workspace.
///
/// Two independent defenses are applied:
/// 1. lexical rejection of absolute paths and `..` segments, before touching
///    the filesystem;
/// 2. canonicalization + prefix check, which catches symlinks pointing outside
///    the (already canonical) workspace root.
fn resolve_existing(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidArgs("path must not be empty".to_string()));
    }

    let relative = Path::new(raw);
    reject_lexical_escapes(relative)?;

    let joined = ctx.workspace.join(relative);
    let canonical = std::fs::canonicalize(&joined).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError::InvalidArgs(format!("path does not exist: {raw}"))
        } else {
            ToolError::Io(error)
        }
    })?;

    if !canonical.starts_with(&ctx.workspace) {
        return Err(ToolError::Denied(format!(
            "path escapes the workspace: {raw}"
        )));
    }

    Ok(canonical)
}

pub(crate) fn resolve_existing_file(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    let path = resolve_existing(ctx, raw)?;
    if !path.is_file() {
        return Err(ToolError::InvalidArgs(format!(
            "not a regular file: {raw}"
        )));
    }
    Ok(path)
}

pub(crate) fn resolve_existing_dir(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    let path = resolve_existing(ctx, raw)?;
    if !path.is_dir() {
        return Err(ToolError::InvalidArgs(format!(
            "not a directory: {raw}"
        )));
    }
    Ok(path)
}

fn reject_lexical_escapes(path: &Path) -> Result<(), ToolError> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(ToolError::Denied(format!(
                    "path must not contain '..': {}",
                    path.display()
                )))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::Denied(format!(
                    "absolute paths are not allowed: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_segments_and_absolute_paths() {
        assert!(matches!(
            reject_lexical_escapes(Path::new("../escape.txt")),
            Err(ToolError::Denied(_))
        ));
        assert!(matches!(
            reject_lexical_escapes(Path::new("a/../../b")),
            Err(ToolError::Denied(_))
        ));
        assert!(matches!(
            reject_lexical_escapes(Path::new("/etc/passwd")),
            Err(ToolError::Denied(_))
        ));
        assert!(reject_lexical_escapes(Path::new("a/b.txt")).is_ok());
        assert!(reject_lexical_escapes(Path::new("./a")).is_ok());
    }
}
