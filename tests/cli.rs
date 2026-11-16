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
        .arg("--direct")
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
    for name in [
        "updates",
        "message",
        "request_approval",
        "decide_approval",
        "approvals",
        "approval",
    ] {
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

#[test]
fn approval_cli_rejects_missing_choices_and_malformed_target_before_network() {
    let bin = env!("CARGO_BIN_EXE_cc-sakura");
    for args in [
        vec!["approval", "request", "owner/room", "Deploy?"],
        vec!["approval", "decide", "owner/room", "wrong", "not-a-uuid"],
    ] {
        let result = Command::new(bin).args(args).output().unwrap();
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("read client profile"));
    }
    assert!(
        Command::new(bin)
            .args(["approval", "--help"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn separate_proxy_processes_connect_to_one_daemon() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = broker_profile(dir.path());
    let socket = dir.path().join("broker.sock");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .args(["daemon", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(socket.exists());
    let mut proxies = Vec::new();
    for _ in 0..3 {
        let mut proxy = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
            .arg("--config")
            .arg(&profile)
            .args(["proxy", "--socket"])
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            proxy.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"})
        )
        .unwrap();
        let mut output = BufReader::new(proxy.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["serverInfo"]["name"], "cc-sakura");
        proxies.push(proxy);
    }
    assert!(daemon.try_wait().unwrap().is_none());
    for mut proxy in proxies {
        proxy.kill().unwrap();
        proxy.wait().unwrap();
    }
    let status = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .args(["daemon", "status", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["profiles"], 1);
    let stopped = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
        .args(["daemon", "stop", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(daemon.wait().unwrap().success());
    assert!(!socket.exists());
}

#[cfg(unix)]
fn broker_profile(dir: &std::path::Path) -> std::path::PathBuf {
    let key = dir.join("agent.pem");
    assert!(
        Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
            .args(["auth", "keygen", "--out"])
            .arg(&key)
            .output()
            .unwrap()
            .status
            .success()
    );
    let profile = dir.join("room.toml");
    std::fs::write(
        &profile,
        format!(
            r#"project_url="https://example.org/owner/room"
[auth]
mode="signed"
server_id="00000000-0000-4000-8000-000000000001"
project_id="00000000-0000-4000-8000-000000000002"
member_id="00000000-0000-4000-8000-000000000003"
private_key_file={:?}
"#,
            key.to_str().unwrap()
        ),
    )
    .unwrap();
    profile
}

#[cfg(unix)]
#[test]
fn simultaneous_mcp_startup_uses_one_daemon_and_restart_changes_pid() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .prefix("sakura-auto-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = broker_profile(dir.path());
    let mut proxies = Vec::new();
    let mut replies = Vec::new();
    for _ in 0..5 {
        let mut proxy = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
            .env("TMPDIR", dir.path())
            .arg("--config")
            .arg(&profile)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            proxy.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"})
        )
        .unwrap();
        let output = proxy.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line);
            send.send((result, line)).ok();
        });
        proxies.push(proxy);
        replies.push(receive);
    }
    for reply in replies {
        let (result, line) = reply
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap();
        assert!(result.unwrap() > 0);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["serverInfo"]["name"], "cc-sakura");
    }
    let control = |action: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_cc-sakura"))
            .env("TMPDIR", dir.path())
            .args(["daemon", action])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .last()
            .unwrap()
    };
    let before = control("status");
    assert_eq!(before["profiles"], 1);
    assert_eq!(control("start")["pid"], before["pid"]);
    for mut proxy in proxies {
        proxy.kill().unwrap();
        proxy.wait().unwrap();
    }
    let after = control("restart");
    assert_ne!(before["pid"], after["pid"]);
    assert_eq!(after["profiles"], 0);
    control("stop");
}
