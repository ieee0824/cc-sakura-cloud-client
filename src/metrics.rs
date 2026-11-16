//! Local aggregate counters; never include room names, payloads or credentials.
use std::sync::atomic::{AtomicU64, Ordering};
pub static STREAM_STARTS: AtomicU64 = AtomicU64::new(0);
pub static STREAMS_ACTIVE: AtomicU64 = AtomicU64::new(0);
pub static RECONNECTS: AtomicU64 = AtomicU64::new(0);
pub static HISTORY_BACKFILLS: AtomicU64 = AtomicU64::new(0);
pub static KEY_FETCHES: AtomicU64 = AtomicU64::new(0);
pub static SHARED_READS: AtomicU64 = AtomicU64::new(0);
pub fn snapshot() -> serde_json::Value {
    serde_json::json!({
        "stream_starts":STREAM_STARTS.load(Ordering::Relaxed),"streams_active":STREAMS_ACTIVE.load(Ordering::Relaxed),
        "reconnects":RECONNECTS.load(Ordering::Relaxed),"history_backfills":HISTORY_BACKFILLS.load(Ordering::Relaxed),
        "key_fetches":KEY_FETCHES.load(Ordering::Relaxed),"shared_reads":SHARED_READS.load(Ordering::Relaxed),
        "ring_bytes":crate::subscription::ring_bytes(),"rpc_in_flight":8-crate::limits::rpc().available_permits(),
        "receive_waiters":64-crate::limits::receive().available_permits()
    })
}
