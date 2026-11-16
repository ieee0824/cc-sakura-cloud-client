//! Explicit, profile-scoped local MCP broker. Never retries an IPC write.
use crate::{Client, Config};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

fn directory(path: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.permissions().mode() & 0o077 == 0,
        "socket parent must be a private directory (0700)"
    );
    ensure!(
        m.uid() == current_uid()?,
        "socket directory belongs to another user"
    );
    Ok(())
}
fn current_uid() -> Result<u32> {
    let file = tempfile::NamedTempFile::new()?;
    Ok(file.as_file().metadata()?.uid())
}
fn socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    directory(path.parent().context("socket needs parent")?)?;
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.file_type().is_socket()
            && m.uid() == current_uid()?
            && m.permissions().mode() & 0o077 == 0,
        "unsafe broker socket"
    );
    Ok(())
}
pub async fn serve(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "socket path must be absolute");
    directory(path.parent().context("socket needs parent")?)?;
    let lock_path = path.with_extension("lock");
    let mut options = std::fs::OpenOptions::new();
    use std::os::unix::fs::OpenOptionsExt;
    options.read(true).write(true).create(true).mode(0o600);
    if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
        ensure!(
            meta.is_file() && meta.uid() == current_uid()? && meta.mode() & 0o077 == 0,
            "unsafe broker lock"
        );
    }
    let lock = options.open(&lock_path)?;
    lock.try_lock().context("broker already running")?;
    if path.exists() {
        socket(path)?;
        ensure!(
            UnixStream::connect(path).await.is_err(),
            "broker already listening"
        );
        std::fs::remove_file(path)?;
    }
    let registry = Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::<
        String,
        Client,
    >::new()));
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let uid = current_uid()?;
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(128));
    let consumers = Arc::new(tokio::sync::Semaphore::new(64));
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let mut sessions = tokio::task::JoinSet::new();
    let mut last_activity = tokio::time::Instant::now();
    let mut idle = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (stream, _) = connection?;
                if stream.peer_cred()?.uid() != uid { continue; }
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                last_activity=tokio::time::Instant::now();
                let registry = registry.clone();
                let shutdown=shutdown.clone();
                let consumers=consumers.clone();
                sessions.spawn(async move {
                    let _permit = permit;
                    // Never log profile paths, payloads or raw authentication errors.
                    let _ = session_control(stream, registry, Some(shutdown),Some(consumers)).await;
                });
            }
            _ = sessions.join_next(), if !sessions.is_empty() => { last_activity=tokio::time::Instant::now(); }
            _ = idle.tick() => {
                if sessions.is_empty() && last_activity.elapsed()>=std::time::Duration::from_secs(600) {break;}
            }
            _ = shutdown.notified() => {break;}
            _ = tokio::signal::ctrl_c() => {
                // Remove only our socket while the listener still owns it.
                break;
            }
        }
    }
    drop(listener);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    std::fs::remove_file(path)?;
    Ok(())
}
#[cfg(test)]
async fn session(
    stream: UnixStream,
    registry: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Client>>>,
) -> Result<()> {
    session_control(stream, registry, None, None).await
}
async fn session_control(
    stream: UnixStream,
    registry: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Client>>>,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    consumers: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<()> {
    let (input, mut output) = stream.into_split();
    let mut input = BufReader::new(input);
    let mut greeting = String::new();
    let count = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        (&mut input).take(16385).read_line(&mut greeting),
    )
    .await??;
    ensure!(count <= 16384, "broker greeting too large");
    let greeting: Value = serde_json::from_str(&greeting)?;
    if (greeting["version"] != env!("CARGO_PKG_VERSION") && greeting.get("command").is_none())
        || greeting["protocol"] != 1
    {
        output
            .write_all(b"{\"error\":\"broker version mismatch; restart daemon\"}\n")
            .await?;
        return Ok(());
    }
    if let Some(command) = greeting["command"].as_str() {
        ensure!(
            matches!(command, "status" | "stop"),
            "invalid daemon command"
        );
        let registry = registry.lock().await;
        let mut subscriptions = 0;
        for client in registry.values() {
            subscriptions += client.subscriptions.lock().await.len();
        }
        let mut retry_after = 0;
        for client in registry.values() {
            if let Some(until) = client.reconnect.lock().await.not_before {
                retry_after = retry_after.max(
                    until
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_secs(),
                );
            }
        }
        let response = json!({"ok":true,"version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),"profiles":registry.len(),"subscriptions":subscriptions,"connections":consumers.as_ref().map(|limit|64-limit.available_permits()),"retry_after_seconds":retry_after,"metrics":crate::metrics::snapshot()});
        output.write_all(format!("{response}\n").as_bytes()).await?;
        output.flush().await?;
        if command == "stop" {
            if let Some(shutdown) = shutdown {
                shutdown.notify_one();
            }
        }
        return Ok(());
    }
    let _consumer = match consumers {
        Some(limit) => Some(
            limit
                .try_acquire_owned()
                .context("too many broker consumers")?,
        ),
        None => None,
    };
    let profile =
        std::path::PathBuf::from(greeting["profile"].as_str().context("missing profile")?);
    ensure!(profile.is_absolute(), "profile must be absolute");
    let text = std::fs::read_to_string(&profile)?;
    ensure!(text.len() <= 1024 * 1024, "profile too large");
    let config: Config = toml::from_str(&text)?;
    let mut digest = Sha256::new();
    digest.update(profile.as_os_str().as_encoded_bytes());
    digest.update(text.as_bytes());
    if let Some(auth) = &config.auth {
        digest.update(std::fs::read(&auth.private_key_file)?);
        if let Some(path) = &auth.recovery_key_file {
            digest.update(std::fs::read(path)?);
        }
    }
    let prefix = format!("{}\0", profile.display());
    let scope = format!("{prefix}{:x}", digest.finalize());
    let client = {
        let mut registry = registry.lock().await;
        registry.retain(|key, client| {
            if key.starts_with(&prefix) && key != &scope {
                client
                    .invalidated
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                false
            } else {
                true
            }
        });
        if let Some(client) = registry.get(&scope) {
            client.clone()
        } else {
            if registry.len() >= 16 {
                registry.retain(|_, client| Arc::strong_count(&client.invalidated) > 1);
            }
            ensure!(registry.len() < 16, "too many broker profiles");
            let client = crate::client_from_config(&config, None, None)?;
            registry.insert(scope, client.clone());
            client
        }
    };
    output.write_all(b"{\"ok\":true}\n").await?;
    crate::mcp::run_session(client, input, output, true).await
}
pub fn runtime_socket() -> Result<std::path::PathBuf> {
    let parent = std::env::temp_dir().join(format!("cc-sakura-{}", current_uid()?));
    if !parent.exists() {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    directory(&parent)?;
    Ok(parent.join("broker.sock"))
}
pub async fn connect(path: &Path, profile: &Path, auto: bool) -> Result<BufReader<UnixStream>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut started = false;
    let stream = loop {
        if path.exists() {
            socket(path)?;
        }
        match UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(error) => {
                if !auto || tokio::time::Instant::now() >= deadline {
                    return Err(error.into());
                }
                if !started {
                    {
                        use std::os::unix::process::CommandExt;
                        let mut command = std::process::Command::new(std::env::current_exe()?);
                        command.process_group(0);
                        command
                    }
                    .arg("daemon")
                    .arg("--socket")
                    .arg(path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                    started = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };
    ensure!(
        stream.peer_cred()?.uid() == current_uid()?,
        "broker user mismatch"
    );
    let mut stream = BufReader::new(stream);
    let greeting = json!({"protocol":1,"version":env!("CARGO_PKG_VERSION"),"profile":std::fs::canonicalize(profile)?});
    stream
        .get_mut()
        .write_all(format!("{greeting}\n").as_bytes())
        .await?;
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_line(&mut response),
    )
    .await??;
    let response: Value = serde_json::from_str(&response)?;
    ensure!(
        response["ok"] == true,
        "broker attach failed; check profile or restart daemon"
    );
    Ok(stream)
}
async fn control(path: &Path, command: &str) -> Result<Value> {
    socket(path)?;
    let stream = UnixStream::connect(path).await?;
    ensure!(
        stream.peer_cred()?.uid() == current_uid()?,
        "broker user mismatch"
    );
    let mut stream = BufReader::new(stream);
    let request = json!({"protocol":1,"version":env!("CARGO_PKG_VERSION"),"command":command});
    stream
        .get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .await?;
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_line(&mut line),
    )
    .await??;
    let response: Value = serde_json::from_str(&line)?;
    ensure!(response["ok"] == true, "daemon command rejected");
    Ok(response)
}
pub async fn manage(path: &Path, action: &crate::DaemonAction) -> Result<()> {
    use crate::DaemonAction;
    match action {
        DaemonAction::Serve => return serve(path).await,
        DaemonAction::Status => {
            println!("{}", control(path, "status").await?);
            return Ok(());
        }
        DaemonAction::Stop | DaemonAction::Restart => {
            println!("{}", control(path, "stop").await?);
            for _ in 0..240 {
                if !path.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            ensure!(
                !path.exists(),
                "daemon still stopping; wait before starting another"
            );
            if matches!(action, DaemonAction::Stop) {
                return Ok(());
            }
        }
        DaemonAction::Start => {
            if let Ok(response) = control(path, "status").await {
                println!("{response}");
                return Ok(());
            }
        }
    }
    {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command.process_group(0);
        command
    }
    .args(["daemon", "serve", "--socket"])
    .arg(path)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()?;
    for _ in 0..200 {
        if let Ok(response) = control(path, "status").await {
            println!("{response}");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("daemon did not start")
}

pub async fn call(profile: &Path, name: &str, args: Value) -> Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    call_once(profile, name, args, &id).await.with_context(|| {
        format!("broker call {id}; for an uncertain post use auth call-status / auth retry-call")
    })
}
async fn call_once(profile: &Path, name: &str, args: Value, call_id: &str) -> Result<Value> {
    let mut stream = connect(&runtime_socket()?, profile, true).await?;
    for (id, method, params) in [
        (1, "initialize", json!({})),
        (
            2,
            "broker/call",
            json!({"name":name,"arguments":args,"call_id":call_id}),
        ),
    ] {
        if id == 2 {
            stream
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await?;
        }
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        stream
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        let mut line = String::new();
        let count = tokio::time::timeout(
            std::time::Duration::from_secs(330),
            (&mut stream)
                .take((crate::framing::MAX_FRAME + 1) as u64)
                .read_line(&mut line),
        )
        .await??;
        ensure!(
            count > 0,
            "broker connection closed; check outbox before retrying a post"
        );
        let response: Value = serde_json::from_str(&line)?;
        ensure!(
            response["id"] == id && response.get("error").is_none(),
            "invalid broker response"
        );
        if id == 2 {
            if let Some(error) = response["result"].get("error") {
                if let Some(status) = error["status"].as_u64() {
                    return Err(crate::signed::StreamFailure {
                        status: status as u16,
                        retry_after: error["retry_after"].as_u64().unwrap_or(0),
                        code: error["code"].as_str().map(str::to_owned),
                    }
                    .into());
                }
                anyhow::bail!(
                    "{}",
                    error["message"]
                        .as_str()
                        .unwrap_or("broker operation failed")
                );
            }
            return Ok(response["result"]["value"].clone());
        }
    }
    unreachable!()
}

pub async fn proxy(path: &Path, profile: &Path, auto: bool) -> Result<()> {
    let stream = connect(path, profile, auto).await?;
    let (mut input, mut output) = tokio::io::split(stream);
    let mut stdin = crate::framing::Frames::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result=async {
            while let Some(line)=stdin.next_line().await? {
                let mut request:Value=serde_json::from_str(&line)?;
                if request["method"]=="tools/call" {request["_broker_call_id"]=json!(uuid::Uuid::new_v4().to_string());}
                output.write_all(format!("{request}\n").as_bytes()).await?;
            }
            output.shutdown().await?;
            Ok::<_,anyhow::Error>(())
        }=>{result?;}
        result=tokio::io::copy(&mut input,&mut stdout)=>{result?;}
    }
    // IPC failure is never replayed as a new posting operation.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn multiple_sessions_share_one_profile_without_loading_keys_in_proxy() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
        let key = temp.path().join("agent.pem");
        crate::signed::generate_key(&key)?;
        let profile = temp.path().join("profile.toml");
        crate::signed::save_private_profile(
            &profile,
            &format!(
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
        )?;
        let path = temp.path().join("broker.sock");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let registry = Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));
        let server_registry = registry.clone();
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..12 {
                let (stream, _) = listener.accept().await.unwrap();
                let registry = server_registry.clone();
                tasks.spawn(async move { session(stream, registry).await });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap().unwrap();
            }
        });
        let mut connections = Vec::new();
        for _ in 0..10 {
            let mut stream = connect(&path, &profile, false).await?;
            stream
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
                .await?;
            let mut line = String::new();
            stream.read_line(&mut line).await?;
            let response: Value = serde_json::from_str(&line)?;
            assert_eq!(response["result"]["serverInfo"]["name"], "cc-sakura");
            connections.push(stream);
        }
        assert_eq!(registry.lock().await.len(), 1);
        let original = registry.lock().await.values().next().unwrap().clone();
        let second = temp.path().join("another.toml");
        let text = std::fs::read_to_string(&profile)?;
        let second_text = text.replace(
            "00000000-0000-4000-8000-000000000003",
            "00000000-0000-4000-8000-000000000004",
        );
        crate::signed::save_private_profile(&second, &second_text)?;
        connections.push(connect(&path, &second, false).await?);
        assert_eq!(registry.lock().await.len(), 2);
        assert!(
            !original
                .invalidated
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        // A changed profile replaces its old generation, while the other identity stays independent.
        std::fs::write(&profile, text.replace("/owner/room", "/owner/changed"))?;
        connections.push(connect(&path, &profile, false).await?);
        assert_eq!(registry.lock().await.len(), 2);
        assert!(
            original
                .invalidated
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(
            original
                .receive(serde_json::json!({"after":0}))
                .await
                .is_err()
        );
        drop(connections);
        task.await?;
        Ok(())
    }
    #[test]
    fn rejects_public_directories_and_non_socket_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
        directory(temp.path())?;
        let path = temp.path().join("broker.sock");
        std::fs::write(&path, "not a socket")?;
        assert!(socket(&path).is_err());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))?;
        assert!(directory(temp.path()).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn accepts_private_socket_and_rejects_symlink() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
        let path = temp.path().join("broker.sock");
        let _listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        socket(&path)?;
        let link = temp.path().join("link.sock");
        std::os::unix::fs::symlink(&path, &link)?;
        assert!(socket(&link).is_err());
        Ok(())
    }
}
