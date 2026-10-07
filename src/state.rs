use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::rules::Tier;

/// What one run leaves for the next. A single writer is safe: launchd never overlaps runs of one job.
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// `Last-Modified` of the last full notifications response, sent back as `If-Modified-Since`.
    pub last_modified: Option<String>,
    /// Unix seconds before which GitHub asks not to poll again (`X-Poll-Interval`).
    pub next_poll_at: u64,
    /// `since` for the next poll, ISO 8601.
    pub since: Option<String>,
    /// Keyed by thread id.
    pub threads: HashMap<String, Thread>,
    /// CODEOWNERS verdict per repo; config overrides are never cached.
    pub owned: HashMap<String, Owned>,
}

#[derive(Serialize, Deserialize)]
pub struct Thread {
    pub updated_at: String,
    /// `None` when no rule matched.
    pub tier: Option<Tier>,
    pub rule: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Owned {
    pub owned: bool,
    pub checked_at: u64,
}

impl State {
    /// `None` when there is no state file yet.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
            )),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Written to a temp file and renamed, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`, the format of GitHub's timestamps, so the two compare
/// as strings.
pub fn iso(secs: u64) -> String {
    let secs = i64::try_from(secs).expect("timestamp fits i64");
    DateTime::from_timestamp(secs, 0)
        .expect("timestamp in range")
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_iso() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_791_374_399), "2026-10-07T11:59:59Z");
        assert_eq!(iso(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn save_then_load() {
        let path = std::env::temp_dir().join(format!("gh-tamis-{}/state.json", std::process::id()));
        assert!(State::load(&path).unwrap().is_none());
        let mut s = State::default();
        s.threads.insert(
            "1".into(),
            Thread {
                updated_at: iso(0),
                tier: Some(Tier::Notify),
                rule: Some("mention".into()),
            },
        );
        s.save(&path).unwrap();
        let back = State::load(&path).unwrap().unwrap();
        assert_eq!(back.threads["1"].tier, Some(Tier::Notify));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
