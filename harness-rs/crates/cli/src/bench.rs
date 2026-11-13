//! `aqp-agent-v1` headless mode: one benchmark run over stdin/stdout.
//!
//! The benchmark starts this binary inside a task container and writes one
//! `prompt` record carrying the task text and the model selected for this run.
//! We answer with `agent_start`, an optional `message` carrying the turn text,
//! `agent_end`, and a terminal `done`. Only `done` means the run finished; a
//! `stop` record cancels the in-flight turn.
//!
//! Trust boundary: the workspace is task input and may be untrusted, so this
//! mode never auto-discovers `harn.toml` from it. The base URL and capability
//! come from the operator/benchmark environment (`AQP_GATEWAY_URL`,
//! `AQP_GATEWAY_TOKEN`) or explicit flags.
//!
//! Human input is not requested yet: Phase 1 tools run without approval, so no
//! `input_request` is emitted. `answer` records are accepted and ignored.
#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use dshrs_agent::AgentLoop;
use dshrs_kernel::EventBus;
use dshrs_llm::OpenAiCompatible;
use dshrs_session::{SessionLog, StopReason};
use dshrs_tools::{ToolContext, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::mpsc;

type Error = Box<dyn std::error::Error>;

/// Everything resolved before the prompt arrives. The model is intentionally
/// absent: it comes from the `prompt` record so one binary can serve a whole
/// benchmark model matrix.
pub struct BenchConfig {
    pub cwd: PathBuf,
    pub base_url: String,
    pub api_key: Option<String>,
    pub default_model: String,
    pub session_path: PathBuf,
    pub system_prompt: String,
    pub max_steps: usize,
    pub redactions: Vec<String>,
}

pub async fn run(config: BenchConfig) -> Result<(), Error> {
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

    // Wait for the single prompt record. A stop received before the prompt ends
    // the session without running a turn.
    let (prompt, model) = loop {
        let Some(line) = receiver.recv().await else {
            return Ok(());
        };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("prompt") => {
                let model = record
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| !model.is_empty())
                    .unwrap_or(&config.default_model)
                    .to_string();
                let prompt = record
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                break (prompt, model);
            }
            Some("stop") => return Ok(()),
            _ => {}
        }
    };

    eprintln!(
        "harn bench-protocol: model={model} base_url={} session={}",
        config.base_url,
        config.session_path.display()
    );

    let provider = OpenAiCompatible::new(
        config.base_url.clone(),
        model.clone(),
        config.api_key.clone(),
    )?;
    let context = ToolContext::new(config.cwd.clone())?;
    let session_log = SessionLog::open(config.session_path.clone())?;
    let agent = AgentLoop::new(
        Arc::new(provider),
        ToolRegistry::with_defaults(),
        session_log,
        context,
    )
    .with_max_steps(config.max_steps)
    .with_system_prompt(config.system_prompt)
    .with_redactions(config.redactions)
    .with_event_bus(EventBus::default());

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
