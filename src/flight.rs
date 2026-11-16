//! Share only overlapping, identical reads; never persist a response cache.
use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, OnceCell};
#[derive(Clone)]
struct Failure {
    message: String,
    status: Option<u16>,
    retry: u64,
    code: Option<String>,
}
impl Failure {
    fn capture(error: anyhow::Error) -> Self {
        let typed = error.downcast_ref::<crate::signed::StreamFailure>();
        Self {
            message: format!("{error:#}"),
            status: typed.map(|e| e.status),
            retry: typed.map_or(0, |e| e.retry_after),
            code: typed.and_then(|e| e.code.clone()),
        }
    }
    fn restore(&self) -> anyhow::Error {
        if let Some(status) = self.status {
            crate::signed::StreamFailure {
                status,
                retry_after: self.retry,
                code: self.code.clone(),
            }
            .into()
        } else {
            anyhow::anyhow!("{}", self.message)
        }
    }
}
struct Flight {
    result: OnceCell<std::result::Result<Value, Failure>>,
    changed: Notify,
}
#[derive(Default)]
pub struct Flights {
    pending: Mutex<std::collections::BTreeMap<String, Weak<Flight>>>,
}
impl Flights {
    pub async fn run<F>(&self, key: String, task: F) -> Result<Value>
    where
        F: std::future::Future<Output = Result<Value>> + Send + 'static,
    {
        let (flight, start) = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("shared reads unavailable"))?;
            pending.retain(|_, f| f.strong_count() > 0);
            if let Some(flight) = pending.get(&key).and_then(Weak::upgrade) {
                (flight, false)
            } else {
                anyhow::ensure!(pending.len() < 128, "too many pending shared reads");
                let flight = Arc::new(Flight {
                    result: OnceCell::new(),
                    changed: Notify::new(),
                });
                pending.insert(key, Arc::downgrade(&flight));
                (flight, true)
            }
        };
        if !start {
            crate::metrics::SHARED_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if start {
            let flight = flight.clone();
            tokio::spawn(async move {
                let result = task.await.map_err(Failure::capture);
                let _ = flight.result.set(result);
                flight.changed.notify_waiters();
            });
        }
        loop {
            let notified = flight.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = flight.result.get() {
                return result.as_ref().map(Clone::clone).map_err(Failure::restore);
            }
            notified.await;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_shared_work() -> Result<()> {
        let flights = Arc::new(Flights::default());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let first = flights.clone();
        let executed = count.clone();
        let release = gate.clone();
        let a = tokio::spawn(async move {
            first
                .run("same".into(), async move {
                    executed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    release.notified().await;
                    Ok(serde_json::json!({"id":1}))
                })
                .await
        });
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        a.abort();
        let _ = a.await;
        let second = flights.clone();
        let executed = count.clone();
        let b = tokio::spawn(async move {
            second
                .run("same".into(), async move {
                    executed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Value::Null)
                })
                .await
        });
        tokio::task::yield_now().await;
        gate.notify_one();
        assert_eq!(b.await??["id"], 1);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A later, non-overlapping read is fresh.
        assert_eq!(
            flights
                .run("same".into(), async { Ok(serde_json::json!({"id":2})) })
                .await?["id"],
            2
        );
        Ok(())
    }
}
