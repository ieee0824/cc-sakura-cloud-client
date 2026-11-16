//! Process-wide budgets shared by every identity in the broker.
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
pub fn rpc() -> &'static Semaphore {
    static LIMIT: OnceLock<Semaphore> = OnceLock::new();
    LIMIT.get_or_init(|| Semaphore::new(8))
}
pub fn receive() -> &'static Semaphore {
    static LIMIT: OnceLock<Semaphore> = OnceLock::new();
    LIMIT.get_or_init(|| Semaphore::new(64))
}
pub fn stream(scope: &str) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
    static ALL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static KEYS: OnceLock<Mutex<std::collections::BTreeMap<String, Arc<Semaphore>>>> =
        OnceLock::new();
    let all = ALL
        .get_or_init(|| Arc::new(Semaphore::new(32)))
        .clone()
        .try_acquire_owned()
        .context("too many upstream streams")?;
    let mut keys = KEYS
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("stream limits unavailable"))?;
    keys.retain(|_, limit| limit.available_permits() != 4 || Arc::strong_count(limit) > 1);
    let key = keys
        .entry(scope.to_owned())
        .or_insert_with(|| Arc::new(Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .context("too many streams for this key")?;
    Ok((all, key))
}
