//! One reader per normalized subscription; explicit caller cursors never advance each other.
use crate::Client;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};

pub(crate) fn ring_bytes() -> usize {
    RING_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}
static RING_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct PageBudget(usize);
impl PageBudget {
    #[allow(deprecated)] // fetch_update also supports older Rust toolchains.
    fn reserve(bytes: usize) -> Option<Self> {
        RING_BYTES
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |used| {
                    used.checked_add(bytes)
                        .filter(|next| *next <= 64 * 1024 * 1024)
                },
            )
            .ok()
            .map(|_| Self(bytes))
    }
}
impl Drop for PageBudget {
    fn drop(&mut self) {
        RING_BYTES.fetch_sub(self.0, std::sync::atomic::Ordering::SeqCst);
    }
}
struct Page {
    start: u64,
    value: Arc<Value>,
    bytes: usize,
    _budget: PageBudget,
}
impl Page {
    fn new(start: u64, value: Arc<Value>, bytes: usize) -> Option<Self> {
        Some(Self {
            start,
            value,
            bytes,
            _budget: PageBudget::reserve(bytes)?,
        })
    }
    fn end(&self) -> u64 {
        self.value["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_u64())
            .max()
            .unwrap_or(self.start)
    }
}
pub struct Subscription {
    state: Mutex<State>,
    changed: Arc<Notify>,
    active: Arc<std::sync::atomic::AtomicBool>,
    waiters: std::sync::atomic::AtomicUsize,
    last_activity: std::sync::Mutex<tokio::time::Instant>,
}
struct State {
    pages: VecDeque<Page>,
    floor: u64,
    bytes: usize,
    error: Option<String>,
    closed: bool,
}
impl Subscription {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                pages: VecDeque::new(),
                floor: 0,
                bytes: 0,
                error: None,
                closed: false,
            }),
            changed: Arc::new(Notify::new()),
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            waiters: std::sync::atomic::AtomicUsize::new(0),
            last_activity: std::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }
}
impl Subscription {
    fn idle(&self) -> bool {
        self.waiters.load(std::sync::atomic::Ordering::SeqCst) == 0
            && self
                .last_activity
                .lock()
                .map(|last| last.elapsed() >= Duration::from_secs(60))
                .unwrap_or(true)
    }
}
struct Consumer(Arc<Subscription>);
impl Drop for Consumer {
    fn drop(&mut self) {
        if let Ok(mut last) = self.0.last_activity.lock() {
            *last = tokio::time::Instant::now();
        }
        self.0
            .waiters
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
pub async fn receive(client: &Client, mut args: Value) -> Result<Value> {
    let _permit = crate::limits::receive()
        .try_acquire()
        .context("too many waiting consumers")?;
    if let Some(signed) = &client.signed {
        args = signed.target(&args)?;
        if let Some(after) = args.get("after") {
            anyhow::ensure!(
                after.as_u64().is_some_and(|n| n <= 9007199254740991),
                "invalid after cursor"
            );
        }
        if let Some(limit) = args.get("limit") {
            anyhow::ensure!(
                limit.as_u64().is_some_and(|n| (1..=1000).contains(&n)),
                "invalid limit"
            );
        }
        let human = args
            .get("include_human")
            .map(|v| v.as_bool().context("invalid include_human"))
            .transpose()?
            .unwrap_or(true);
        args["include_human"] = json!(human);
        if args["mentions"].is_null() {
            args.as_object_mut().unwrap().remove("mentions");
        } else {
            let id = args["mentions"].as_str().context("invalid mentions")?;
            anyhow::ensure!(
                uuid::Uuid::parse_str(id)?.to_string() == id,
                "invalid mentions"
            );
        }
    }
    let seconds = args["timeout_seconds"]
        .as_u64()
        .unwrap_or(300)
        .clamp(1, 300);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let after = args["after"].as_u64().unwrap_or(0);
    args.as_object_mut()
        .context("invalid receive arguments")?
        .remove("timeout_seconds");
    let mut normalized = args.clone();
    normalized.as_object_mut().unwrap().remove("after");
    normalized.as_object_mut().unwrap().remove("limit");
    let key = serde_json::to_string(&normalized)?;
    let slot = {
        let mut slots = client.subscriptions.lock().await;
        if let Some(slot) = slots.get(&key) {
            slot.clone()
        } else {
            if slots.len() >= 4 {
                bail!("too many receive subscriptions");
            }
            let slot = Arc::new(Subscription::new());
            slot.state.lock().await.floor = after;
            slots.insert(key.clone(), slot.clone());
            let worker = client.clone();
            let target = slot.clone();
            let mut upstream = normalized;
            upstream["after"] = json!(after);
            upstream["limit"] = json!(100);
            upstream["timeout_seconds"] = json!(60);
            tokio::spawn(async move {
                let mut cursor = after;
                loop {
                    if worker.check_valid().is_err() {
                        let mut state = target.state.lock().await;
                        state.error = Some("profile changed; reconnect".into());
                        state.closed = true;
                        target.changed.notify_waiters();
                        break;
                    }
                    let result = worker
                        .receive_observed(
                            upstream.clone(),
                            Some((target.active.clone(), target.changed.clone())),
                        )
                        .await;
                    let mut state = target.state.lock().await;
                    match result {
                        Ok(page) => {
                            let end = page["messages"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|m| m["id"].as_u64())
                                .max()
                                .unwrap_or(cursor);
                            if end > cursor {
                                let bytes = page.to_string().len();
                                let value = Arc::new(page);
                                if let Some(page) = Page::new(cursor, value, bytes) {
                                    state.pages.push_back(page);
                                    state.bytes += bytes;
                                } else {
                                    state.pages.clear();
                                    state.bytes = 0;
                                    state.floor = end;
                                }
                                while state.pages.len() > 10 || state.bytes > 16 * 1024 * 1024 {
                                    if let Some(page) = state.pages.pop_front() {
                                        state.floor = page.end();
                                        state.bytes -= page.bytes;
                                    } else {
                                        break;
                                    }
                                }
                                cursor = end;
                                upstream["after"] = json!(cursor);
                            }
                        }
                        Err(error) if crate::transient(&error) => {
                            state.pages.clear();
                            state.bytes = 0;
                            target
                                .active
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            target.changed.notify_waiters();
                            drop(state);
                            worker.note_stream_error(&error).await;
                            let _ = tokio::time::timeout(
                                Duration::from_secs(60),
                                worker.reconnect_wait(),
                            )
                            .await;
                            if target.idle() {
                                break;
                            }
                            continue;
                        }
                        Err(error) => {
                            state.error = Some(error.to_string());
                            state.pages.clear();
                            state.bytes = 0;
                            state.closed = true;
                        }
                    }
                    target.changed.notify_waiters();
                    if state.closed || target.idle() {
                        state.closed = true;
                        break;
                    }
                }
                target
                    .active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                target.changed.notify_waiters();
                let mut direct_key = upstream;
                direct_key.as_object_mut().unwrap().remove("after");
                direct_key
                    .as_object_mut()
                    .unwrap()
                    .remove("timeout_seconds");
                if let Ok(direct_key) = serde_json::to_string(&direct_key) {
                    worker.receivers.lock().await.remove(&direct_key);
                }
                // Keep terminal failures until explicit profile reload / daemon restart.
                if target.state.lock().await.error.is_none() {
                    let mut slots = worker.subscriptions.lock().await;
                    if slots.get(&key).is_some_and(|v| Arc::ptr_eq(v, &target)) {
                        slots.remove(&key);
                    }
                }
            });
            slot
        }
    };
    slot.waiters
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _consumer = Consumer(slot.clone());
    loop {
        client.check_valid()?;
        let notified = slot.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = slot.state.lock().await;

            if let Some(error) = &state.error {
                bail!("{error}");
            }
            if slot.active.load(std::sync::atomic::Ordering::SeqCst) {
                if after < state.floor {
                    drop(state);
                    crate::metrics::HISTORY_BACKFILLS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return client.call("history", args).await;
                }
                for (index, cached) in state.pages.iter().enumerate() {
                    let mut page = (*cached.value).clone();
                    if let Some(messages) = page["messages"].as_array_mut() {
                        messages.retain(|m| m["id"].as_u64().is_some_and(|id| id > after));
                        let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
                        if !messages.is_empty() {
                            let more = messages.len() > limit;
                            messages.truncate(limit);
                            if more || index + 1 < state.pages.len() {
                                page["has_more"] = json!(true);
                            }
                            return Ok(page);
                        }
                    }
                }
            }
            if state.closed {
                bail!("subscription closed; retry with the same cursor");
            }
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            // No extra cloud request just because an AI's wait ended.
            client.check_valid()?;
            if !slot.active.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::signed::StreamFailure {
                    status: 503,
                    retry_after: 1,
                    code: Some("subscription_reconnecting".into()),
                }
                .into());
            }
            return Ok(json!({"messages":[],"has_more":false}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn client() -> Client {
        Client {
            subscriptions: Default::default(),
            #[cfg(unix)]
            remote: None,
            profile_auth: None,
            invalidated: Default::default(),
            receivers: Default::default(),
            reconnect: Default::default(),
            http: reqwest::Client::new(),
            signed: None,
        }
    }
    #[tokio::test]
    async fn ten_waiters_receive_one_network_stream_without_consuming_each_other() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted = count.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut request = [0u8; 4096];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            gate.notified().await;
            let frame =
                "event: messages\ndata: {\"messages\":[{\"id\":17}],\"has_more\":false}\n\n";
            socket
                .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let response = reqwest::get(format!("http://{address}/events")).await?;
        let client = client();
        client.receivers.lock().await.insert(
            "{\"limit\":100}".into(),
            Arc::new(Mutex::new(crate::Receiver {
                stream: Some(crate::Events {
                    expires_at: None,
                    expiry: None,
                    _permits: None,
                    observer: None,
                    opened: std::time::Instant::now(),
                    response,
                    buffer: vec![],
                    handshake: None,
                    verifier: None,
                }),
                after: 0,
                last: None,
            })),
        );
        let mut readers = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let client = client.clone();
            readers
                .spawn(async move { client.receive(json!({"after":0,"timeout_seconds":5})).await });
        }
        tokio::task::yield_now().await;
        release.notify_one();
        while let Some(result) =
            tokio::time::timeout(Duration::from_secs(6), readers.join_next()).await?
        {
            assert_eq!(result??["messages"][0]["id"], 17);
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(client.subscriptions.lock().await.len(), 1);
        assert_eq!(client.receivers.lock().await.len(), 1);
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn readers_have_independent_cursors_and_limits() -> Result<()> {
        let client = client();
        let slot = Arc::new(Subscription::new());
        slot.active.store(true, std::sync::atomic::Ordering::SeqCst);
        slot.state.lock().await.pages.push_back(
            Page::new(
                0,
                Arc::new(json!({"messages":[{"id":2},{"id":7}],"has_more":false})),
                100,
            )
            .unwrap(),
        );
        client.subscriptions.lock().await.insert("{}".into(), slot);
        let (a, b) = tokio::join!(
            client.receive(json!({"after":0,"limit":1})),
            client.receive(json!({"after":0,"limit":2}))
        );
        let a = a?;
        let b = b?;
        assert_eq!(a["messages"], json!([{"id":2}]));
        assert_eq!(a["has_more"], true);
        assert_eq!(b["messages"], json!([{"id":2},{"id":7}]));
        assert_eq!(
            client.receive(json!({"after":2})).await?["messages"],
            json!([{"id":7}])
        );
        assert_eq!(
            client.receive(json!({"after":0})).await?["messages"],
            b["messages"]
        );
        Ok(())
    }
    #[tokio::test]
    async fn cached_posts_are_not_returned_while_connection_is_inactive() -> Result<()> {
        let client = client();
        let slot = Arc::new(Subscription::new());
        slot.state
            .lock()
            .await
            .pages
            .push_back(Page::new(0, Arc::new(json!({"messages":[{"id":12}]})), 30).unwrap());
        client
            .subscriptions
            .lock()
            .await
            .insert("{}".into(), slot.clone());
        let waiting = client.clone();
        let task = tokio::spawn(async move { waiting.receive(json!({"after":0})).await });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!task.is_finished());
        slot.state.lock().await.error = Some("access revoked".into());
        slot.changed.notify_waiters();
        assert!(
            task.await?
                .unwrap_err()
                .to_string()
                .contains("access revoked")
        );
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_of_one_wait_does_not_close_other_readers() -> Result<()> {
        let client = client();
        let slot = Arc::new(Subscription::new());
        slot.active.store(true, std::sync::atomic::Ordering::SeqCst);
        client
            .subscriptions
            .lock()
            .await
            .insert("{}".into(), slot.clone());
        let first = client.clone();
        let task = tokio::spawn(async move { first.receive(json!({"after":0})).await });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        slot.state
            .lock()
            .await
            .pages
            .push_back(Page::new(0, Arc::new(json!({"messages":[{"id":9}]})), 40).unwrap());
        slot.changed.notify_waiters();
        assert_eq!(
            client.receive(json!({"after":0})).await?["messages"][0]["id"],
            9
        );
        Ok(())
    }
}
