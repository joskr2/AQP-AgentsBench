//! Agent loop: turns input into steps over a log.
//!
//! A *turn* is one user input. A turn is made of *steps*: each step rebuilds
//! the prompt from the session log, streams one provider response, and either
//! finishes the turn or executes the requested tools and starts another step.
//!
//! History is never kept in memory as the source of truth: every step calls
//! [`SessionLog::read_all`] and re-derives messages, which is what makes the
//! log authoritative (the DSH property this skeleton copies).
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dshrs_kernel::{EventBus, HarnessEvent};
use dshrs_llm::{LlmDelta, LlmProvider, LlmRequest, Message, ToolCall, ToolSchema};
use dshrs_session::{derive_messages, SessionEvent, SessionLog, StopReason};
use dshrs_tools::{ToolContext, ToolRegistry};
use futures_util::StreamExt;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_STEPS: usize = 16;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Session(#[from] dshrs_session::SessionError),
    #[error(transparent)]
    Llm(#[from] dshrs_llm::LlmError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Assistant text produced during the turn (steps joined with newlines).
    pub text: String,
    pub stop_reason: StopReason,
    /// Number of provider calls made.
    pub steps: usize,
}

/// The turn/step loop. Cheap to build; one instance per session.
pub struct AgentLoop {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    log: Mutex<SessionLog>,
    context: ToolContext,
    max_steps: usize,
    system_prompt: Option<String>,
    cancellation: CancellationToken,
    event_bus: Option<EventBus>,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: ToolRegistry,
        log: SessionLog,
        context: ToolContext,
    ) -> Self {
        Self {
            provider,
            tools,
            log: Mutex::new(log),
            context,
            max_steps: DEFAULT_MAX_STEPS,
            system_prompt: None,
            cancellation: CancellationToken::new(),
            event_bus: None,
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Handle to the token used by this loop (cloned into a signal handler).
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Runs one turn to completion. Tool failures are reported to the model as
    /// tool results; only provider/session errors abort the turn.
    pub async fn run_turn(&self, input: &str) -> Result<TurnOutcome, AgentError> {
        self.ensure_session_started().await?;
        self.append(SessionEvent::UserMessage {
            text: input.to_string(),
        })
        .await?;
        self.emit(HarnessEvent::TurnStarted);

        let mut steps = 0_usize;
        let mut text_chunks: Vec<String> = Vec::new();

        let stop_reason: StopReason = 'turn: loop {
            if self.cancellation.is_cancelled() {
                break 'turn StopReason::Cancelled;
            }
            if steps >= self.max_steps {
                break 'turn StopReason::MaxSteps;
            }
            steps += 1;
            self.emit(HarnessEvent::StepStarted { step: steps });

            let request = LlmRequest {
                messages: self.messages().await?,
                tools: self.tool_schemas(),
            };

            let stream_result: Option<Result<dshrs_llm::DeltaStream, dshrs_llm::LlmError>> =
                tokio::select! {
                    _ = self.cancellation.cancelled() => None,
                    result = self.provider.stream(request) => Some(result),
                };
            let Some(stream_result) = stream_result else {
                break 'turn StopReason::Cancelled;
            };
            let mut stream = match stream_result {
                Ok(stream) => stream,
                Err(error) => {
                    self.append(SessionEvent::TurnFinished {
                        stop_reason: StopReason::Error,
                    })
                    .await?;
                    return Err(AgentError::Llm(error));
                }
            };

            let mut step_text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut stream_cancelled = false;
            loop {
                tokio::select! {
                    _ = self.cancellation.cancelled() => {
                        stream_cancelled = true;
                        break;
                    }
                    item = stream.next() => match item {
                        None => break,
                        Some(Err(error)) => {
                            self.append(SessionEvent::TurnFinished {
                                stop_reason: StopReason::Error,
                            })
                            .await?;
                            return Err(AgentError::Llm(error));
                        }
                        Some(Ok(LlmDelta::Text(text))) => {
                            step_text.push_str(&text);
                            self.emit(HarnessEvent::TextDelta { text });
                        }
                        Some(Ok(LlmDelta::ToolCall(call))) => calls.push(call),
                        Some(Ok(LlmDelta::Finished)) => {}
                    }
                }
            }
            if stream_cancelled {
                break 'turn StopReason::Cancelled;
            }

            if !step_text.is_empty() {
                text_chunks.push(step_text.clone());
                self.append(SessionEvent::AssistantText { text: step_text })
                    .await?;
            }

            if calls.is_empty() {
                break 'turn StopReason::EndTurn;
            }

            for call in calls {
                if self.cancellation.is_cancelled() {
                    break 'turn StopReason::Cancelled;
                }
                self.append(SessionEvent::ToolCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await?;
                self.emit(HarnessEvent::ToolCallRequested {
                    name: call.name.clone(),
                });

                let outcome: Option<Result<String, dshrs_tools::ToolError>> =
                    tokio::select! {
                        _ = self.cancellation.cancelled() => None,
                        result = self.tools.execute(&call.name, call.arguments.clone(), &self.context) => Some(result),
                    };
                let (ok, content) = match outcome {
                    Some(Ok(content)) => (true, content),
                    Some(Err(error)) => (false, format!("tool error: {error}")),
                    None => {
                        // Keep the log consistent: every tool call gets a result.
                        self.append(SessionEvent::ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            ok: false,
                            content: "cancelled".to_string(),
                        })
                        .await?;
                        break 'turn StopReason::Cancelled;
                    }
                };

                self.emit(HarnessEvent::ToolCallFinished {
                    name: call.name.clone(),
                    ok,
                });
                self.append(SessionEvent::ToolResult {
                    call_id: call.id,
                    name: call.name,
                    ok,
                    content,
                })
                .await?;
            }
        };

        self.append(SessionEvent::TurnFinished { stop_reason }).await?;
        if stop_reason == StopReason::Cancelled {
            self.emit(HarnessEvent::Cancelled);
        }
        self.emit(HarnessEvent::TurnFinished {
            stop_reason: stop_reason_label(stop_reason).to_string(),
        });

        Ok(TurnOutcome {
            text: text_chunks.join("\n"),
            stop_reason,
            steps,
        })
    }

    async fn ensure_session_started(&self) -> Result<(), AgentError> {
        let started = self
            .log
            .lock()
            .await
            .read_all()?
            .iter()
            .any(|event| matches!(event, SessionEvent::SessionStarted { .. }));
        if started {
            return Ok(());
        }

        let path = {
            let log = self.log.lock().await;
            log.path().display().to_string()
        };
        self.append(SessionEvent::SessionStarted {
            session_id: new_session_id(),
            model: self.provider.model().to_string(),
            cwd: self.context.workspace().display().to_string(),
        })
        .await?;
        self.emit(HarnessEvent::SessionOpened { path });
        Ok(())
    }

    /// The prompt: system configuration (not history) + everything derived from
    /// the log.
    async fn messages(&self) -> Result<Vec<Message>, AgentError> {
        let events = self.log.lock().await.read_all()?;
        let mut messages = Vec::new();
        if let Some(system_prompt) = &self.system_prompt {
            messages.push(Message::system(system_prompt.as_str()));
        }
        messages.extend(derive_messages(&events));
        Ok(messages)
    }

    fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }

    async fn append(&self, event: SessionEvent) -> Result<(), AgentError> {
        self.log.lock().await.append(&event)?;
        Ok(())
    }

    fn emit(&self, event: HarnessEvent) {
        if let Some(event_bus) = &self.event_bus {
            event_bus.emit(event);
        }
    }
}

fn stop_reason_label(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxSteps => "max_steps",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use dshrs_kernel::EventBus;
    use dshrs_llm::{FakeProvider, FakeTurn, ToolCall};
    use dshrs_tools::{ToolContext, ToolRegistry};
    use serde_json::json;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "dshrs-agent-{}-{nanos}-{unique}-{label}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn session_path(&self) -> PathBuf {
            self.root.join("session.jsonl")
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn read_tool_call(id: &str) -> FakeTurn {
        FakeTurn::with_tool_calls(vec![ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": "hello.txt" }),
        }])
    }

    #[tokio::test]
    async fn executes_a_tool_call_then_finishes_the_turn() {
        let workspace = TempWorkspace::new("turn");
        std::fs::write(workspace.root().join("hello.txt"), "hello world\n").unwrap();

        let provider = Arc::new(FakeProvider::new(
            "fake-model",
            vec![read_tool_call("call-1"), FakeTurn::text("listo")],
        ));
        let context = ToolContext::new(workspace.root()).unwrap();
        let log = SessionLog::open(workspace.session_path()).unwrap();
        let bus = EventBus::default();
        let mut receiver = bus.subscribe();

        let agent = AgentLoop::new(
            provider.clone(),
            ToolRegistry::with_defaults(),
            log,
            context,
        )
        .with_max_steps(4)
        .with_event_bus(bus);

        let outcome = agent.run_turn("lee hello.txt").await.unwrap();

        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(outcome.text, "listo");
        assert_eq!(outcome.steps, 2);
        assert_eq!(provider.calls(), 2);

        let events = SessionLog::open(workspace.session_path())
            .unwrap()
            .read_all()
            .unwrap();
        assert_eq!(
            events.len(),
            6,
            "expected session_started, user, tool call, tool result, assistant text, turn finished"
        );
        assert!(matches!(&events[0], SessionEvent::SessionStarted { .. }));
        assert!(matches!(
            &events[1],
            SessionEvent::UserMessage { text } if text.as_str() == "lee hello.txt"
        ));
        match &events[2] {
            SessionEvent::ToolCall { name, .. } => assert_eq!(name.as_str(), "read"),
            other => panic!("unexpected event: {other:?}"),
        }
        match &events[3] {
            SessionEvent::ToolResult { ok, content, .. } => {
                assert!(ok);
                assert!(content.contains("hello world"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(
            &events[4],
            SessionEvent::AssistantText { text } if text.as_str() == "listo"
        ));
        assert!(matches!(
            &events[5],
            SessionEvent::TurnFinished {
                stop_reason: StopReason::EndTurn
            }
        ));

        let mut saw_tool_call = false;
        let mut saw_turn_finished = false;
        while let Ok(event) = receiver.try_recv() {
            match event {
                HarnessEvent::ToolCallRequested { name } => {
                    assert_eq!(name.as_str(), "read");
                    saw_tool_call = true;
                }
                HarnessEvent::TurnFinished { stop_reason } => {
                    assert_eq!(stop_reason.as_str(), "end_turn");
                    saw_turn_finished = true;
                }
                _ => {}
            }
        }
        assert!(saw_tool_call, "tool call event was not published");
        assert!(saw_turn_finished, "turn finished event was not published");
    }

    #[tokio::test]
    async fn stops_at_max_steps_without_exceeding_the_budget() {
        let workspace = TempWorkspace::new("max-steps");
        std::fs::write(workspace.root().join("hello.txt"), "hello world\n").unwrap();

        let script: Vec<FakeTurn> = (0..3)
            .map(|index| read_tool_call(&format!("call-{index}")))
            .collect();
        let provider = Arc::new(FakeProvider::new("fake-model", script));
        let context = ToolContext::new(workspace.root()).unwrap();
        let log = SessionLog::open(workspace.session_path()).unwrap();

        let agent = AgentLoop::new(provider.clone(), ToolRegistry::with_defaults(), log, context)
            .with_max_steps(2);

        let outcome = agent.run_turn("loop forever").await.unwrap();

        assert_eq!(outcome.stop_reason, StopReason::MaxSteps);
        assert_eq!(outcome.steps, 2);
        assert_eq!(provider.calls(), 2);

        let events = SessionLog::open(workspace.session_path())
            .unwrap()
            .read_all()
            .unwrap();
        let tool_calls = events
            .iter()
            .filter(|event| matches!(event, SessionEvent::ToolCall { .. }))
            .count();
        assert_eq!(tool_calls, 2, "the loop must not exceed max_steps");
        assert!(matches!(
            events.last(),
            Some(SessionEvent::TurnFinished {
                stop_reason: StopReason::MaxSteps
            })
        ));
    }
}
