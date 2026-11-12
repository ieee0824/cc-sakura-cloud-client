//! SSE watcher. Durable state contains IDs only, never decrypted messages.
use crate::{Client, transient};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;

#[derive(Args)]
pub struct Options {
    pub room: String,
    #[arg(long)]
    pub state: PathBuf,
    /// New state starts at the current tail unless this cursor is supplied.
    #[arg(long,value_parser=clap::value_parser!(i64).range(0..))]
    pub after: Option<i64>,
    /// Absolute executable path. Receives one JSON event on stdin; no shell is used.
    #[arg(long)]
    pub handler: Option<PathBuf>,
    #[arg(long, allow_hyphen_values = true)]
    pub handler_arg: Vec<String>,
    #[arg(long,default_value_t=180,value_parser=clap::value_parser!(u64).range(1..=3600))]
    pub handler_timeout: u64,
    #[arg(long,default_value_t=10,value_parser=clap::value_parser!(u32).range(1..=1000))]
    pub max_events_per_hour: u32,
    #[arg(long)]
    pub include_agents: bool,
    /// Resolve an uncertain previous dispatch without running it again.
    #[arg(long, conflicts_with = "retry_pending")]
    pub ack_pending: bool,
    /// Explicitly rerun an uncertain dispatch; inspect the room/outbox first.
    #[arg(long)]
    pub retry_pending: bool,
}
fn private_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Reject symlinks and public state files instead of broadening permissions.
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        ensure!(meta.is_file(), "watch state must be a regular file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                meta.permissions().mode() & 0o077 == 0,
                "watch state must have private permissions (0600)"
            );
        }
    }
    Ok(options.open(path)?)
}
struct WatchLock(File);
impl Drop for WatchLock {
    fn drop(&mut self) {
        // Explicitly release on Linux even if a concurrent fork temporarily
        // inherited the open file description before exec closes its copy.
        let _ = self.0.unlock();
    }
}
struct State {
    db: Connection,
    _lock: WatchLock,
}
impl State {
    fn open(path: &Path, scope: &str) -> Result<Self> {
        ensure!(path.is_absolute(), "--state must be absolute");
        let lock = private_file(&path.with_extension("watch-lock"))?;
        lock.try_lock()
            .context("another watcher is using this state")?;
        let lock = WatchLock(lock);
        private_file(path)?;
        let db = Connection::open(path)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE IF NOT EXISTS own_posts(id INTEGER PRIMARY KEY);CREATE TABLE IF NOT EXISTS dispatches(at INTEGER NOT NULL);CREATE INDEX IF NOT EXISTS dispatches_at ON dispatches(at)")?;
        let previous: Option<String> = db
            .query_row("SELECT value FROM state WHERE key='scope'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(previous) = previous {
            ensure!(
                previous == scope,
                "watch state belongs to another identity or room"
            );
        } else {
            db.execute("INSERT INTO state VALUES('scope',?1)", [scope])?;
        }
        Ok(Self { db, _lock: lock })
    }
    fn get(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .db
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM state WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn set(&self, key: &str, value: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO state VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }
    fn own(&self, id: i64) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM own_posts WHERE id=?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }
    fn remember(&self, id: i64) -> Result<()> {
        self.db
            .execute("INSERT OR IGNORE INTO own_posts VALUES(?1)", [id])?;
        self.db.execute("DELETE FROM own_posts WHERE id<(SELECT id FROM own_posts ORDER BY id DESC LIMIT 1 OFFSET 9999)",[])?;
        Ok(())
    }
    fn ack(&self, id: i64) -> Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.set("cursor", id)?;
            self.db
                .execute("DELETE FROM state WHERE key='pending'", [])?;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                self.db.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                Err(e)
            }
        }
    }
    fn reserve(&self, id: i64, max: u32) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.db
            .execute("DELETE FROM dispatches WHERE at<=?1", [now - 3600])?;
        let n: i64 = self
            .db
            .query_row("SELECT count(*) FROM dispatches", [], |r| r.get(0))?;
        ensure!(
            n < i64::from(max),
            "watch hourly dispatch limit reached; cursor preserved"
        );
        self.set("pending", id)?;
        self.db
            .execute("INSERT INTO dispatches VALUES(?1)", [now])?;
        Ok(())
    }
}
fn refs(message: &Value) -> Vec<i64> {
    let mut ids = Vec::new();
    for field in ["reply_to", "replies_to"] {
        let value = &message["payload"][field];
        let values = value
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]);
        for v in values.into_iter().take(8) {
            if let Some(id) = v.as_i64().or_else(|| v.as_str()?.parse().ok()) {
                if id > 0 && id < message["id"].as_i64().unwrap_or(0) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids.truncate(8);
    ids
}
fn eligible(message: &Value, me: &str, include_agents: bool) -> bool {
    message["sender"] != me
        && message["auth"]["scheme"] != "legacy"
        && (include_agents || message["is_agent"] != true)
}
fn safe_message(m: &Value) -> Value {
    let mut m = m.clone();
    crate::mcp::compact_message(&mut m);
    json!({"id":m["id"],"sender":m["sender"],"handle":m["handle"],"created_at":m["created_at"],"payload":m["payload"],"mentions":m["mentions"],"is_agent":m["is_agent"],"auth":{"scheme":m["auth"]["scheme"],"verified":m["auth"]["verified"],"server_authenticated":m["auth"]["server_authenticated"],"decrypted":m["auth"]["decrypted"]}})
}
async fn dispatch(options: &Options, event: &Value) -> Result<()> {
    if let Some(program) = &options.handler {
        let directory = tempfile::tempdir()?;
        let mut child = tokio::process::Command::new(program)
            .args(&options.handler_arg)
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("start watch handler")?;
        let mut input = child.stdin.take().context("handler stdin unavailable")?;
        let payload = serde_json::to_vec(event)?;
        let exchange = async {
            input.write_all(&payload).await?;
            input.shutdown().await?;
            drop(input);
            let status = child.wait().await?;
            ensure!(
                status.success(),
                "watch handler failed; inspect room/outbox before resolving pending dispatch"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(options.handler_timeout), exchange)
            .await
            .context(
                "handler timed out; inspect room/outbox before resolving pending dispatch",
            )??;
    } else {
        let mut stdout = tokio::io::stdout();
        stdout
            .write_all(serde_json::to_string(event)?.as_bytes())
            .await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}
async fn handle(
    client: &Client,
    options: &Options,
    state: &State,
    message: &Value,
    me: &str,
) -> Result<()> {
    let id = message["id"].as_i64().context("missing message ID")?;
    if message["sender"] == me {
        state.remember(id)?;
        state.ack(id)?;
        return Ok(());
    }
    if !eligible(message, me, options.include_agents) {
        state.ack(id)?;
        return Ok(());
    }
    let mentioned = message["mentions"]
        .as_array()
        .is_some_and(|items| items.iter().any(|id| id == me));
    let mut reply = false;
    for parent in refs(message) {
        if state.own(parent)? {
            reply = true;
            break;
        }
        if !mentioned {
            let page = client
                .call(
                    "history",
                    json!({"room":options.room,"after":parent-1,"limit":1,"include_human":false}),
                )
                .await?;
            if let Some(found) = page["messages"]
                .as_array()
                .and_then(|items| items.iter().find(|m| m["id"] == parent))
            {
                if found["sender"] == me {
                    state.remember(parent)?;
                    reply = true;
                    break;
                }
            }
        }
    }
    if !mentioned && !reply {
        state.ack(id)?;
        return Ok(());
    }
    // Fetch context only for addressed requests, not idle ticks or every room post.
    let context = client
        .call(
            "history",
            json!({"room":options.room,"before":id,"limit":20,"include_human":false}),
        )
        .await?;
    let mut context = context["messages"].as_array().cloned().unwrap_or_default();
    // A reply may refer to an old post outside the nearby page. Keep that
    // explicit evidence, even when our persisted ID index already knows it.
    for parent in refs(message) {
        if context.iter().any(|m| m["id"] == parent) {
            continue;
        }
        let page = client
            .call(
                "history",
                json!({"room":options.room,"after":parent-1,"limit":1,"include_human":false}),
            )
            .await?;
        if let Some(found) = page["messages"]
            .as_array()
            .and_then(|items| items.iter().find(|m| m["id"] == parent))
        {
            context.push(found.clone());
            if context.len() > 20 {
                context.remove(0);
            }
        }
    }
    let event = json!({"v":1,"event_id":format!("{}:{}",client.signed.as_ref().unwrap().auth.project_id,id),"room":options.room,"self_member_id":me,"reason":if mentioned{"mention"}else{"reply"},"message":safe_message(message),"context":context.iter().map(safe_message).collect::<Vec<_>>()});
    state.reserve(id, options.max_events_per_hour)?;
    dispatch(options, &event).await?;
    state.ack(id)?;
    Ok(())
}
pub async fn run(client: &Client, options: Options) -> Result<()> {
    if let Some(handler) = &options.handler {
        ensure!(
            handler.is_absolute(),
            "--handler must be an absolute executable path"
        );
    }
    ensure!(
        options.handler.is_some() || options.handler_arg.is_empty(),
        "--handler-arg requires --handler"
    );
    let auth = &client
        .signed
        .as_ref()
        .context("signed profile required")?
        .auth;
    let scope = json!([
        auth.server_id,
        auth.project_id,
        auth.member_id,
        options.room
    ])
    .to_string();
    let state = State::open(&options.state, &scope)?;
    let snapshot=client.call("history",json!({"room":options.room,"before":9007199254740991_i64,"limit":1000,"include_human":false})).await?;
    let messages = snapshot["messages"]
        .as_array()
        .context("missing messages")?;
    for m in messages {
        if m["sender"] == auth.member_id {
            state.remember(m["id"].as_i64().context("missing ID")?)?;
        }
    }
    if state.get("cursor")?.is_none() {
        state.set(
            "cursor",
            options.after.unwrap_or_else(|| {
                messages
                    .iter()
                    .filter_map(|m| m["id"].as_i64())
                    .max()
                    .unwrap_or(0)
            }),
        )?;
    } else {
        ensure!(options.after.is_none(), "--after only applies to new state");
    }
    if let Some(pending) = state.get("pending")? {
        if options.ack_pending {
            state.ack(pending)?;
        } else if options.retry_pending {
            let page = client
                .call(
                    "history",
                    json!({"room":options.room,"after":pending-1,"limit":1,"include_human":false}),
                )
                .await?;
            let message = page["messages"]
                .as_array()
                .and_then(|items| items.iter().find(|m| m["id"] == pending))
                .context("pending message unavailable")?;
            handle(client, &options, &state, message, &auth.member_id).await?;
        } else {
            bail!(
                "uncertain pending dispatch #{pending}; inspect room/outbox, then use --ack-pending or --retry-pending"
            );
        }
    }
    loop {
        let args = json!({"room":options.room,"after":state.get("cursor")?.unwrap_or(0),"limit":1000,"include_human":false});
        let stream = client.events(&args).await;
        let mut stream = match stream {
            Ok(Some(stream)) => stream,
            Ok(None) => bail!("signed SSE required"),
            Err(e) if transient(&e) || e.to_string() == "event connection timed out" => {
                eprintln!("watch reconnect");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        loop {
            match stream.next().await {
                Ok(Some(page)) => {
                    for message in page["messages"].as_array().context("missing messages")? {
                        if message["id"].as_i64().context("missing message ID")?
                            > state.get("cursor")?.unwrap_or(0)
                        {
                            handle(client, &options, &state, message, &auth.member_id).await?;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) if transient(&e) || e.to_string() == "event heartbeat timed out" => break,
                Err(e) => return Err(e),
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_survives_restart_is_scoped_and_blocks_concurrent_watchers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("watch.sqlite3");
        let state = State::open(&path, "scope")?;
        state.set("cursor", 10)?;
        state.remember(7)?;
        state.reserve(11, 1)?;
        assert!(State::open(&path, "scope").is_err());
        assert!(state.reserve(12, 1).is_err());
        assert_eq!(state.get("pending")?, Some(11));
        drop(state);
        assert!(State::open(&path, "other room").is_err());
        let state = State::open(&path, "scope")?;
        assert_eq!(state.get("cursor")?, Some(10));
        assert!(state.own(7)?);
        assert_eq!(state.get("pending")?, Some(11));
        state.ack(11)?;
        assert_eq!(state.get("pending")?, None);
        assert_eq!(state.get("cursor")?, Some(11));
        Ok(())
    }
    #[test]
    fn filters_self_agents_legacy_and_bounds_reply_lookups() {
        let base = json!({"id":30,"sender":"human","is_agent":false,"auth":{"scheme":"session-v1"},"payload":{"reply_to":[1,2,"3",30,31,-1,0],"replies_to":4}});
        assert!(eligible(&base, "me", false));
        assert_eq!(refs(&base), vec![1, 2, 3, 4]);
        let mut m = base.clone();
        m["sender"] = json!("me");
        assert!(!eligible(&m, "me", true));
        m = base.clone();
        m["is_agent"] = json!(true);
        assert!(!eligible(&m, "me", false));
        assert!(eligible(&m, "me", true));
        m = base;
        m["auth"]["scheme"] = json!("legacy");
        assert!(!eligible(&m, "me", true));
    }
    #[test]
    fn event_excludes_credentials_and_proof_bytes() {
        let m = json!({"id":1,"payload":{"message":"hello"},"session":"private-marker","encrypted_payload":{"ciphertext":"private-marker"},"auth":{"proof_jws":"private-marker","receipt":{"action_json":"private-marker"},"verified":true}});
        let safe = safe_message(&m);
        assert!(!safe.to_string().contains("private-marker"));
        assert_eq!(safe["auth"]["verified"], true);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn handler_uses_stdin_and_failure_preserves_pending() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let program = dir.path().join("handler with spaces.py");
        let output = dir.path().join("event.json");
        std::fs::write(
            &program,
            format!(
                "#!/usr/bin/env python3\nimport sys,pathlib\npathlib.Path({:?}).write_text(sys.stdin.read())\nsys.exit(1)\n",
                output.to_str().unwrap()
            ),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let state = State::open(&dir.path().join("state"), "scope")?;
        state.set("cursor", 4)?;
        state.reserve(5, 10)?;
        let options = Options {
            room: "owner/room".into(),
            state: dir.path().join("state"),
            after: None,
            handler: Some(program),
            handler_arg: vec![],
            handler_timeout: 5,
            max_events_per_hour: 10,
            include_agents: false,
            ack_pending: false,
            retry_pending: false,
        };
        let event = json!({"message":{"id":5,"payload":{"message":"$(do not execute)"}}});
        assert!(dispatch(&options, &event).await.is_err());
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(output)?)?,
            event
        );
        assert_eq!(state.get("cursor")?, Some(4));
        assert_eq!(state.get("pending")?, Some(5));
        Ok(())
    }
}
