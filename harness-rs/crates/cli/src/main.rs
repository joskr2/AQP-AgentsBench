//! `harn` - interactive and one-shot front-end for the harness skeleton.
//!
//! Configuration precedence: flags > environment variables > `harn.toml` >
//! defaults. API keys are read from environment variables only and never
//! written to the session log.
#![forbid(unsafe_code)]

mod bench;
mod config;

use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use config::{Config, ConfigError};
use dshrs_agent::AgentLoop;
use dshrs_kernel::EventBus;
use dshrs_llm::OpenAiCompatible;
use dshrs_session::{SessionLog, StopReason};
use dshrs_tools::{ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

const DEFAULT_MODEL: &str = "deepseek-chat";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_MAX_STEPS: usize = 16;
const DEFAULT_SYSTEM_PROMPT: &str = "You are harn, a small coding agent. Use the tools to inspect \
     and edit files inside the workspace. Prefer small, verifiable steps.";

#[derive(Debug, Parser)]
#[command(name = "harn", version, about = "A minimal Rust coding-agent harness")]
struct Cli {
    /// Model name sent to the OpenAI-compatible endpoint.
    #[arg(long)]
    model: Option<String>,

    /// Base URL of the OpenAI-compatible endpoint (e.g. http://localhost:8080/v1).
    #[arg(long)]
    base_url: Option<String>,

    /// Name of the environment variable holding the API key.
    #[arg(long)]
    api_key_env: Option<String>,

    /// Session log path (JSONL). Defaults to <cwd>/.harn/sessions/<timestamp>.jsonl.
    #[arg(long)]
    session: Option<PathBuf>,

    /// Workspace root for tools and relative paths. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Run a single turn and exit instead of starting the REPL.
    #[arg(long)]
    once: Option<String>,

    /// Optional TOML config file. Defaults to <cwd>/harn.toml when present.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Maximum provider calls (steps) per turn.
    #[arg(long)]
    max_steps: Option<usize>,

    /// Speak the headless aqp-agent-v1 JSONL protocol on stdin/stdout instead of
    /// the REPL or --once prompt. Used by the AQP-AgentsBench runner.
    #[arg(long = "bench-protocol")]
    bench_protocol: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    let cwd = match &cli.cwd {
        Some(path) => std::fs::canonicalize(path)?,
        None => env::current_dir()?,
    };
    // A benchmark workspace is task input and may be untrusted, so protocol
    // mode never advertises or auto-loads a workspace config file.
    let config = load_config(&cli, &cwd, !cli.bench_protocol)?;

    let model = cli
        .model
        .clone()
        .or_else(|| env_string("HARN_MODEL"))
        .or(config.model)
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let base_url = cli
        .base_url
        .clone()
        .or_else(|| {
            if cli.bench_protocol {
                env_string("AQP_GATEWAY_URL")
            } else {
                None
            }
        })
        .or_else(|| env_string("HARN_BASE_URL"))
        .or(config.base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key_env = cli
        .api_key_env
        .clone()
        .or(config.api_key_env)
        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
    let api_key = if cli.bench_protocol {
        // The benchmark hands the container an ephemeral gateway capability.
        env_string("AQP_GATEWAY_TOKEN").or_else(|| env_string(&api_key_env))
    } else {
        env_string(&api_key_env)
    };
    if api_key.is_none() {
        eprintln!(
            "warning: environment variable {api_key_env} is not set; requests are sent without an Authorization header"
        );
    }
    let max_steps = cli
        .max_steps
        .or(config.max_steps)
        .unwrap_or(DEFAULT_MAX_STEPS);
    let system_prompt = config
        .system_prompt
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

    let session_path = match &cli.session {
        Some(path) => path.clone(),
        // Never write harness state into the task workspace: it would pollute
        // the evaluated diff and become model-readable.
        None if cli.bench_protocol => bench_session_path(),
        None => default_session_path(&cwd),
    };

    let mut redactions: Vec<String> = Vec::new();
    if let Some(key) = &api_key {
        redactions.push(key.clone());
    }
    if let Some(token) = env_string("AQP_GATEWAY_TOKEN") {
        redactions.push(token);
    }

    if cli.bench_protocol {
        if cli.once.is_some() {
            return Err("--bench-protocol and --once are mutually exclusive".into());
        }
        return bench::run(bench::BenchConfig {
            cwd,
            base_url,
            api_key,
            default_model: model,
            session_path,
            system_prompt,
            max_steps,
            redactions,
        })
        .await;
    }

    let provider = OpenAiCompatible::new(base_url.clone(), model.clone(), api_key)?;
    let context = ToolContext::new(cwd.clone())?;
    let session_log = SessionLog::open(session_path.clone())?;
    let event_bus = EventBus::default();

    // Ctrl-C cancels the in-flight turn; a second Ctrl-C (or Ctrl-D) ends the REPL.
    let cancellation = CancellationToken::new();
    let signal_token = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_token.cancel();
        }
    });

    let agent = AgentLoop::new(
        Arc::new(provider),
        ToolRegistry::with_defaults(),
        session_log,
        context,
    )
    .with_max_steps(max_steps)
    .with_system_prompt(system_prompt)
    .with_cancellation(cancellation.clone())
    .with_event_bus(event_bus);

    eprintln!(
        "harn: model={model} base_url={base_url} session={}",
        session_path.display()
    );

    let result = match cli.once.as_deref() {
        Some(prompt) => run_once(&agent, prompt).await,
        None => run_repl(&agent).await,
    };
    result?;
    Ok(())
}

fn load_config(cli: &Cli, cwd: &Path, allow_workspace_config: bool) -> Result<Config, ConfigError> {
    match &cli.config {
        Some(path) => Config::load(path),
        None if !allow_workspace_config => Ok(Config::default()),
        None => {
            let default_path = cwd.join("harn.toml");
            if default_path.is_file() {
                Config::load(&default_path)
            } else {
                Ok(Config::default())
            }
        }
    }
}

fn env_string(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn bench_session_path() -> PathBuf {
    std::env::temp_dir()
        .join("harn")
        .join(format!("bench-{}.jsonl", std::process::id()))
}

fn default_session_path(cwd: &Path) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    cwd.join(".harn")
        .join("sessions")
        .join(format!("{millis}.jsonl"))
}

async fn run_once(agent: &AgentLoop, prompt: &str) -> Result<(), Box<dyn Error>> {
    let outcome = agent.run_turn(prompt).await?;
    if !outcome.text.is_empty() {
        println!("{}", outcome.text);
    }
    eprintln!(
        "[stop_reason={} steps={}]",
        stop_reason_label(outcome.stop_reason),
        outcome.steps
    );
    Ok(())
}

async fn run_repl(agent: &AgentLoop) -> Result<(), Box<dyn Error>> {
    eprintln!("harn REPL: type a prompt, or 'exit'/'quit' (Ctrl-D) to leave");

    loop {
        eprint!("harn> ");
        io::stderr().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "exit" | "quit") {
            break;
        }

        let outcome = match agent.run_turn(prompt).await {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("error: {error}");
                continue;
            }
        };

        if !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        if outcome.stop_reason == StopReason::Cancelled {
            eprintln!("[cancelled]");
            break;
        }
        if outcome.stop_reason != StopReason::EndTurn {
            eprintln!(
                "[stop_reason={} steps={}]",
                stop_reason_label(outcome.stop_reason),
                outcome.steps
            );
        }
    }

    Ok(())
}

fn stop_reason_label(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxSteps => "max_steps",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}
