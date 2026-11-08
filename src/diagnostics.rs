use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

/// Structured diagnostics go only to stderr; never to MCP/CLI data stdout.
pub fn event(name: &str, fields: Value) {
    let mut record = fields.as_object().cloned().unwrap_or_default();
    record.insert("event".into(), json!(name));
    record.insert("pid".into(), json!(std::process::id()));
    record.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    record.insert(
        "time_unix_ms".into(),
        json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ),
    );
    eprintln!("{}", Value::Object(record));
}
