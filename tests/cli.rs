use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn only_client_commands_are_exposed() {
    let output = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for command in [
        "auth", "history", "receive", "watch", "mcp", "updates", "message",
    ] {
        assert!(text.contains(command));
    }
    for command in ["serve", "admin", "web"] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
                .arg(command)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn stdio_mcp_initializes_with_a_saved_signed_profile_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("agent.key");
    let output = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .args(["auth", "keygen", "--out"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(output.status.success());
    let profile = dir.path().join("room.toml");
    let text = format!(
        r#"project_url = "https://example.org/owner/project"
[auth]
mode = "signed"
server_id = "00000000-0000-4000-8000-000000000001"
project_id = "00000000-0000-4000-8000-000000000002"
member_id = "00000000-0000-4000-8000-000000000003"
private_key_file = {:?}
[translation]
provider = "auto"
"#,
        key.to_str().unwrap()
    );
    std::fs::write(&profile, text).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .arg("--config")
        .arg(&profile)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ];
    let mut stdin = child.stdin.take().unwrap();
    for message in messages {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|v| serde_json::from_str(v).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "retry"));
    assert!(tools.iter().any(|tool| tool["name"] == "send"));
    for name in ["updates", "message"] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert!(tool["inputSchema"]["properties"].get("project").is_some());
    }
}

#[test]
fn watch_requires_private_state_and_valid_recovery_options() {
    let bin = env!("CARGO_BIN_EXE_cc-sakura");
    for args in [
        vec!["watch", "owner/room"],
        vec![
            "watch",
            "owner/room",
            "--state",
            "/tmp/state",
            "--ack-pending",
            "--retry-pending",
        ],
        vec![
            "watch",
            "owner/room",
            "--state",
            "/tmp/state",
            "--max-events-per-hour",
            "0",
        ],
    ] {
        let output = Command::new(bin).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("read client profile"));
    }
}
