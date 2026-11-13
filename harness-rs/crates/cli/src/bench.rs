//! `aqp-agent-v1` headless mode: one benchmark run over stdin/stdout.
//!
//! The benchmark starts this binary inside a task container and writes one
//! `prompt` record. We answer with `agent_start`, an optional `message` carrying
//! the turn text, `agent_end`, and a terminal `done`. Only `done` means the run
//! finished; a `stop` record cancels the in-flight turn.
//!
//! Human input is not requested yet: Phase 1 tools run without approval, so no
//! `input_request` is emitted. `answer` records are accepted and ignored.
#![forbid(unsafe_code)]

use std::io::{BufRead, Write};

use dshrs_agent::AgentLoop;
use dshrs_session::StopReason;
use serde_json::{json, Value};
use tokio::sync::mpsc;

type Error = Box<dyn std::error::Error>;

pub async fn run(agent: &AgentLoop) -> Result<(), Error> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<String>();
    std::thread::Builder::new()
        .name("aqp-agent-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => {
                        if sender.send(line).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })?;

    // Wait for the single prompt record; ignore anything before it.
    let prompt = loop {
        let Some(line) = receiver.recv().await else {
            return Ok(());
        };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("prompt") {
            break record
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
    };

    let cancellation = agent.cancellation_token();
    let stop_token = cancellation.clone();
    tokio::spawn(async move {
        while let Some(line) = receiver.recv().await {
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if record.get("type").and_then(Value::as_str) == Some("stop") {
                stop_token.cancel();
                return;
            }
        }
    });

    emit(&json!({"type": "agent_start", "agent_id": "main"}))?;

    let outcome = match agent.run_turn(&prompt).await {
        Ok(outcome) => outcome,
        Err(error) => {
            emit(&json!({"type": "agent_end", "agent_id": "main"}))?;
            emit(&json!({
                "type": "done",
                "stop_reason": "error",
                "error": error.to_string(),
            }))?;
            return Ok(());
        }
    };

    if !outcome.text.is_empty() {
        emit(&json!({"type": "message", "text": outcome.text}))?;
    }
    emit(&json!({"type": "agent_end", "agent_id": "main"}))?;

    let (stop_reason, error) = match outcome.stop_reason {
        StopReason::EndTurn => ("end_turn", None),
        StopReason::MaxSteps => ("error", Some("maximum steps reached")),
        StopReason::Cancelled => ("error", Some("cancelled")),
        StopReason::Error => ("error", Some("provider or session error")),
    };
    let mut done = json!({"type": "done", "stop_reason": stop_reason});
    if let Some(error) = error {
        done["error"] = Value::String(error.to_string());
    }
    emit(&done)?;
    Ok(())
}

fn emit(record: &Value) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, record)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
