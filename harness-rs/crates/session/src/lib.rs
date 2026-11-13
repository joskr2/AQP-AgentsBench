//! Session log: the single source of truth for conversation history.
//!
//! Like DSH, the harness never keeps a mutable "messages" array as the primary
//! state. Every meaningful step is appended to a JSONL file, and the provider
//! prompt is *derived* from that log by [`derive_messages`]. A crashed or
//! relaunched process can rebuild the exact same prompt from disk.
#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use dshrs_llm::{Message, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxSteps,
    Cancelled,
    Error,
}

/// One append-only record in the session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        session_id: String,
        model: String,
        cwd: String,
    },
    UserMessage {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        content: String,
    },
    TurnFinished {
        stop_reason: StopReason,
    },
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session event at line {line} of {path}: {source}")]
    Decode {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("session event encoding failed: {0}")]
    Encode(#[from] serde_json::Error),
}

fn io_error(path: &Path, source: std::io::Error) -> SessionError {
    SessionError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Append-only writer/reader for one JSONL session file.
pub struct SessionLog {
    path: PathBuf,
    file: File,
}

impl SessionLog {
    /// Opens (or creates) the log. Parent directories are created on demand.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| io_error(&path, error))?;
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one event as a single JSON line and flushes it immediately.
    ///
    /// Flushing per event keeps the log trustworthy even if the process is
    /// killed mid-turn; the cost is irrelevant for interactive sessions.
    pub fn append(&mut self, event: &SessionEvent) -> Result<(), SessionError> {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|error| io_error(&self.path, error))?;
        self.file
            .flush()
            .map_err(|error| io_error(&self.path, error))?;
        Ok(())
    }

    /// Reads every event back in append order.
    pub fn read_all(&self) -> Result<Vec<SessionEvent>, SessionError> {
        let file = File::open(&self.path).map_err(|error| io_error(&self.path, error))?;
        let mut events = Vec::new();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| io_error(&self.path, error))?;
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str(&line).map_err(|source| SessionError::Decode {
                path: self.path.clone(),
                line: index + 1,
                source,
            })?;
            events.push(event);
        }
        Ok(events)
    }
}

/// Pure projection from the event log to provider messages.
///
/// This is the whole "history is derived" rule: no IO, no hidden state.
pub fn derive_messages(events: &[SessionEvent]) -> Vec<Message> {
    let mut messages = Vec::new();
    for event in events {
        match event {
            SessionEvent::SessionStarted { .. } | SessionEvent::TurnFinished { .. } => {}
            SessionEvent::UserMessage { text } => messages.push(Message::user(text.as_str())),
            SessionEvent::AssistantText { text } => {
                messages.push(Message::assistant(text.as_str()))
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => messages.push(Message::assistant_tool_calls(vec![ToolCall {
                id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }])),
            SessionEvent::ToolResult {
                call_id, content, ..
            } => messages.push(Message::tool(call_id.as_str(), content.as_str())),
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use dshrs_llm::Role;
    use serde_json::json;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn session_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "dshrs-session-{}-{nanos}-{unique}-{label}.jsonl",
            std::process::id()
        ))
    }

    fn sample_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::SessionStarted {
                session_id: "s-1".to_string(),
                model: "fake-model".to_string(),
                cwd: "/tmp/workspace".to_string(),
            },
            SessionEvent::UserMessage {
                text: "read a.txt".to_string(),
            },
            SessionEvent::AssistantText {
                text: "on it".to_string(),
            },
            SessionEvent::ToolCall {
                call_id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: json!({ "path": "a.txt" }),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".to_string(),
                name: "read".to_string(),
                ok: true,
                content: "hello".to_string(),
            },
            SessionEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]
    }

    #[test]
    fn append_and_read_roundtrip() {
        let path = session_path("roundtrip");
        let events = sample_events();

        let mut log = SessionLog::open(path.clone()).unwrap();
        for event in &events {
            log.append(event).unwrap();
        }

        let read = log.read_all().unwrap();
        assert_eq!(read, events);
        // The file is JSONL: one object per line.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), events.len());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_log_reads_as_empty() {
        let path = session_path("empty");
        let log = SessionLog::open(path.clone()).unwrap();
        assert!(log.read_all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn derive_messages_skips_bookkeeping_and_maps_roles() {
        let messages = derive_messages(&sample_events());

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "read a.txt");
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, "on it");
        assert_eq!(messages[2].role, Role::Assistant);
        assert_eq!(messages[2].tool_calls[0].name, "read");
        assert_eq!(
            messages[2].tool_calls[0].arguments,
            json!({ "path": "a.txt" })
        );
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(messages[3].content, "hello");
    }

    #[test]
    fn derive_messages_tolerates_multiple_turns() {
        let mut events = sample_events();
        events.push(SessionEvent::UserMessage {
            text: "and again".to_string(),
        });
        events.push(SessionEvent::TurnFinished {
            stop_reason: StopReason::MaxSteps,
        });

        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages.last().unwrap().content, "and again");
    }
}
