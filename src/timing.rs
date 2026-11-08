//! Opt-in client timing. Only fixed operation names and durations are logged.
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use uuid::Uuid;

static ENABLED: AtomicBool = AtomicBool::new(false);
tokio::task_local! { static TRACE: Uuid; }

pub fn configure(flag: bool) {
    ENABLED.store(
        flag || std::env::var("SAKURA_TIMING").is_ok_and(|v| v == "1"),
        Ordering::Relaxed,
    );
}
fn operation_name(value: &str) -> &'static str {
    match value {
        "join" => "join",
        "identity" => "identity",
        "send" => "send",
        "history" => "history",
        "receive" => "receive",
        "subscribe" => "subscribe",
        "retry" => "retry",
        "keys" => "keys",
        "codecs" => "codecs",
        "codec" => "codec",
        "encryption-device" => "encryption-device",
        "encryption-key" => "encryption-key",
        "encryption-history" => "encryption-history",
        _ => "other",
    }
}
struct Span {
    start: Instant,
    trace: Uuid,
    phase: &'static str,
    operation: &'static str,
    outcome: &'static str,
}
impl Span {
    fn new(phase: &'static str, operation: &str) -> Self {
        Self {
            start: Instant::now(),
            trace: TRACE.try_with(|v| *v).unwrap_or_else(|_| Uuid::new_v4()),
            phase,
            operation: operation_name(operation),
            outcome: "cancelled",
        }
    }
    fn record(&self) -> Value {
        json!({"trace_id":self.trace,"phase":self.phase,"operation":self.operation,
            "elapsed_ms":self.start.elapsed().as_secs_f64()*1000.0,"outcome":self.outcome})
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        crate::diagnostics::event("client_timing", self.record());
    }
}
pub fn measure<'a, T: 'a>(
    phase: &'static str,
    operation: &'a str,
    future: impl Future<Output = Result<T>> + 'a,
) -> std::pin::Pin<Box<impl Future<Output = Result<T>> + 'a>> {
    Box::pin(measure_enabled(
        ENABLED.load(Ordering::Relaxed),
        phase,
        operation,
        future,
    ))
}
async fn measure_enabled<T>(
    enabled: bool,
    phase: &'static str,
    operation: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    if !enabled {
        return future.await;
    }
    let trace = TRACE.try_with(|v| *v).unwrap_or_else(|_| Uuid::new_v4());
    TRACE
        .scope(trace, async {
            let mut span = Span::new(phase, operation);
            let result = future.await;
            span.outcome = if result.is_ok() { "ok" } else { "error" };
            result
        })
        .await
}
pub fn measure_sync<T>(
    phase: &'static str,
    operation: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if !ENABLED.load(Ordering::Relaxed) {
        return f();
    }
    let mut span = Span::new(phase, operation);
    let result = f();
    span.outcome = if result.is_ok() { "ok" } else { "error" };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn record_is_bounded_and_never_includes_caller_text() {
        let span = Span::new("operation", "untrusted operation contents");
        let record = span.record();
        assert_eq!(record["operation"], "other");
        assert_eq!(record.as_object().unwrap().len(), 5);
        assert!(!record.to_string().contains("untrusted"));
        assert!(record["elapsed_ms"].as_f64().unwrap() >= 0.0);
    }
    #[tokio::test]
    async fn nested_phases_share_a_trace_and_errors_are_unchanged() {
        let error = measure_enabled(true, "operation", "send", async {
            let trace = TRACE.with(|v| *v);
            measure_enabled(true, "http.headers", "identity", async {
                assert_eq!(TRACE.with(|v| *v), trace);
                Err::<(), _>(anyhow::anyhow!("private server response"))
            })
            .await
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "private server response");
        assert!(TRACE.try_with(|v| *v).is_err());
    }
    #[tokio::test]
    async fn disabled_measurement_adds_no_trace_context() {
        assert_eq!(
            measure_enabled(false, "operation", "history", async {
                assert!(TRACE.try_with(|v| *v).is_err());
                Ok(42)
            })
            .await
            .unwrap(),
            42
        );
    }
}
