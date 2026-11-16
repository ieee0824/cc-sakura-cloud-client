//! Origin-wide Retry-After gates. Contains timestamps and error codes only.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
#[derive(Clone, Serialize, Deserialize)]
struct Pause {
    until: i64,
    code: String,
    status: u16,
}
fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
fn directory() -> Result<PathBuf> {
    #[cfg(test)]
    {
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        Ok(DIR
            .get_or_init(|| tempfile::tempdir().unwrap())
            .path()
            .to_owned())
    }
    #[cfg(not(test))]
    {
        crate::paths::state_directory()
    }
}
fn path(origin: &str) -> Result<PathBuf> {
    Ok(directory()?.join(format!(
        "pause-{:x}.json",
        Sha256::digest(origin.as_bytes())
    )))
}

fn read(path: &Path) -> Result<Option<Pause>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && metadata.len() <= 4096,
        "invalid quota state"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "quota state must be private"
        );
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}
pub fn before(origin: &str) -> Result<()> {
    let _guard = lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("quota state unavailable"))?;
    if let Some(pause) = read(&path(origin)?)? {
        let remaining = pause.until - crate::signed::now()?;
        if remaining > 0 {
            return Err(crate::signed::StreamFailure {
                status: pause.status,
                retry_after: remaining as u64,
                code: Some(pause.code),
            }
            .into());
        }
    }
    Ok(())
}
pub fn note(origin: &str, status: u16, code: Option<&str>, retry_after: u64) -> Result<()> {
    if code != Some("d1_daily_quota_exceeded") && status != 429 {
        return Ok(());
    }
    let _guard = lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("quota state unavailable"))?;
    let path = path(origin)?;
    let pause = Pause {
        until: crate::signed::now()? + retry_after.clamp(1, 86400) as i64,
        status,
        code: code.unwrap_or("rate_limited").to_owned(),
    };
    if read(&path)?.is_some_and(|old| old.until >= pause.until) {
        return Ok(());
    }
    let parent = path.parent().context("quota path needs parent")?;
    if !parent.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&pause)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
pub fn stream_start(origin: &str) -> Result<()> {
    static STARTS: OnceLock<Mutex<BTreeMap<String, std::collections::VecDeque<i64>>>> =
        OnceLock::new();
    before(origin)?;
    let mut starts = STARTS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("stream limiter unavailable"))?;
    let now = crate::signed::now()?;
    let times = starts.entry(origin.to_owned()).or_default();
    while times.front().is_some_and(|time| *time <= now - 60) {
        times.pop_front();
    }
    if times.len() >= 10 {
        return Err(crate::signed::StreamFailure {
            status: 429,
            retry_after: 60 - (now - times[0]) as u64,
            code: Some("local_stream_limit".into()),
        }
        .into());
    }
    times.push_back(now);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_is_shared_by_origin_and_survives_memory_reload() -> Result<()> {
        let origin = "https://quota-test.invalid";
        note(origin, 503, Some("d1_daily_quota_exceeded"), 300)?;
        let error = before(origin).unwrap_err();
        let error = error
            .downcast_ref::<crate::signed::StreamFailure>()
            .unwrap();
        assert!(error.retry_after > 290);
        assert_eq!(error.code.as_deref(), Some("d1_daily_quota_exceeded"));
        assert!(before("https://unrelated.invalid").is_ok());
        assert!(read(&path(origin)?)?.is_some());
        note(origin, 503, Some("d1_daily_quota_exceeded"), 1)?;
        assert!(
            before(origin)
                .unwrap_err()
                .downcast_ref::<crate::signed::StreamFailure>()
                .unwrap()
                .retry_after
                > 290
        );
        Ok(())
    }
    #[test]
    fn generic_failure_does_not_pause_an_origin() -> Result<()> {
        note(
            "https://ordinary-failure.invalid",
            503,
            Some("service_unavailable"),
            0,
        )?;
        before("https://ordinary-failure.invalid")
    }
}
