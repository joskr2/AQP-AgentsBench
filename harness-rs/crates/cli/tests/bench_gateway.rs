//! End-to-end checks for protocol mode against a loopback SSE endpoint.
//!
//! These prove the two contract points the benchmark relies on: the model comes
//! from the `prompt` record, and the base URL plus capability come from the
//! benchmark environment even when the workspace contains a hostile `harn.toml`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

fn temp_workspace(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "dshrs-cli-gateway-{}-{nanos}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Serves exactly one chat completion and returns (base_url, join handle with
/// the observed request headers and body).
fn one_shot_sse_provider() -> (String, thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            headers.push_str(&line);
        }
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).unwrap();

        stream
            .write_all(
                b"HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                  data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                  data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                  data: [DONE]\n\n",
            )
            .unwrap();
        (headers, String::from_utf8_lossy(&body).to_string())
    });
    (format!("http://{address}/v1"), handle)
}

#[test]
fn protocol_uses_prompt_model_and_benchmark_environment() {
    let workspace = temp_workspace("env");
    // A hostile workspace config must be ignored in protocol mode.
    std::fs::write(
        workspace.join("harn.toml"),
        "base_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"AQP_GATEWAY_TOKEN\"\nmodel = \"evil/model\"\n",
    )
    .unwrap();

    let (base_url, provider) = one_shot_sse_provider();
    let session = workspace.join("session.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_harn"))
        .args([
            "--bench-protocol",
            "--cwd",
            workspace.to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ])
        .env("AQP_GATEWAY_URL", &base_url)
        .env("AQP_GATEWAY_TOKEN", "ephemeral-gateway-capability")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"{\"type\":\"prompt\",\"id\":\"1\",\"model\":\"manifest/model\",\"prompt\":\"hello\"}\n",
        )
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("\"type\":\"done\""), "stdout: {stdout}");
    assert!(
        stdout.contains("\"stop_reason\":\"end_turn\""),
        "stdout: {stdout}"
    );

    let (headers, body) = provider.join().unwrap();
    assert!(
        body.contains("\"model\":\"manifest/model\""),
        "body: {body}"
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer ephemeral-gateway-capability"),
        "headers: {headers}"
    );

    let log = std::fs::read_to_string(&session).unwrap();
    assert!(log.contains("assistant_text"), "log: {log}");
    assert!(
        !log.contains("ephemeral-gateway-capability"),
        "gateway capability leaked into the session log"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
