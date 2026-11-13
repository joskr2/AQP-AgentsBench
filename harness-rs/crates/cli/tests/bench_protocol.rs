//! Process-level checks for the headless `--bench-protocol` mode.
//!
//! These never touch the network: both cases finish before a provider request
//! would be made.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn temp_workspace(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "dshrs-cli-bench-{}-{nanos}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn stop_before_prompt_exits_cleanly_without_protocol_output() {
    let workspace = temp_workspace("stop-before-prompt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_harn"))
        .args([
            "--bench-protocol",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--cwd",
            workspace.to_str().unwrap(),
            "--session",
            workspace.join("session.jsonl").to_str().unwrap(),
            "--api-key-env",
            "AQP_DEFINITELY_UNSET",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("harn binary should start");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"type\":\"stop\"}\n")
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "exit status: {:?}", output.status);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn bench_protocol_rejects_once_mode() {
    let workspace = temp_workspace("mutually-exclusive");
    let output = Command::new(env!("CARGO_BIN_EXE_harn"))
        .args([
            "--bench-protocol",
            "--once",
            "hi",
            "--cwd",
            workspace.to_str().unwrap(),
            "--session",
            workspace.join("session.jsonl").to_str().unwrap(),
            "--api-key-env",
            "AQP_DEFINITELY_UNSET",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("harn binary should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mutually exclusive"));
    let _ = std::fs::remove_dir_all(&workspace);
}
