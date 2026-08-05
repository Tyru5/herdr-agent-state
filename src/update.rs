//! Update-available check, modeled on herdr-file-viewer's: bounded,
//! read-only, fail-silent. Once per 24h a background thread runs a hardened
//! `git ls-remote --tags` against our own repo, compares the highest stable
//! tag to the compiled version, and reports through the event channel. Any
//! failure degrades to "no update info" — never an error in the UI. Disable
//! entirely with `HERDR_STATE_NO_UPDATE_CHECK`.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::Ev;

/// Setting this env var (to anything) disables the update check.
pub const DISABLE_ENV: &str = "HERDR_STATE_NO_UPDATE_CHECK";

/// Minimum gap between network probes.
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Hard wall-clock bound on the probe (connect/DNS hangs included).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `1.2.3` / `v1.2.3`. Pre-release suffixes ("1.2.3-rc1") are NOT
    /// stable versions and parse as None.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let mut next = || -> Option<u64> {
            let p = parts.next()?;
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok())?
        };
        let v = Version { major: next()?, minor: next()?, patch: next()? };
        parts.next().is_none().then_some(v)
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One `git ls-remote --tags` line → a stable version. Only clean tag refs
/// count (peeled `^{}` refs and non-version tags are skipped).
pub fn parse_tag_ref(line: &str) -> Option<Version> {
    let (_sha, r) = line.split_once('\t')?;
    let tag = r.strip_prefix("refs/tags/")?;
    if tag.ends_with("^{}") {
        return None;
    }
    Version::parse(tag)
}

/// The highest stable version in ls-remote output.
pub fn latest_stable(output: &str) -> Option<Version> {
    output.lines().filter_map(parse_tag_ref).max()
}

/// Enough time elapsed since the last probe? Future timestamps (clock skew /
/// corrupt cache) mean "check now".
pub fn should_check(now_unix: u64, last_check_unix: u64) -> bool {
    last_check_unix > now_unix || now_unix - last_check_unix >= CHECK_INTERVAL_SECS
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("herdr-agent-state").join("update-check.json"))
}

/// (last_check_unix, latest_seen) from the cache; zeros/None when unreadable.
fn read_cache() -> (u64, Option<Version>) {
    let Some(path) = cache_path() else { return (0, None) };
    let Ok(text) = std::fs::read_to_string(path) else { return (0, None) };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return (0, None) };
    let last = v.get("last_check_unix").and_then(|n| n.as_u64()).unwrap_or(0);
    let latest = v
        .get("latest_seen")
        .and_then(|s| s.as_str())
        .and_then(Version::parse);
    (last, latest)
}

/// Persist a SUCCESSFUL probe result. Failed probes must not call this — the
/// stale timestamp makes the next launch retry instead of going quiet for 24h.
fn write_cache(now_unix: u64, latest: Option<Version>) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({
        "last_check_unix": now_unix,
        "latest_seen": latest.map(|v| v.to_string()),
    });
    let _ = std::fs::write(path, body.to_string());
}

/// Hardened probe (file-viewer's boundary): run from a freshly-created empty
/// private dir with discovery ceilinged to it (no repo-local `.git/config`
/// can influence it), https transport only, never prompts, killed after
/// `PROBE_TIMEOUT`.
fn probe(repo_url: &str) -> io::Result<String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("herdr-agent-state-probe-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir)?; // exclusive: fails if the path already exists
    let result = probe_in(repo_url, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn probe_in(repo_url: &str, dir: &std::path::Path) -> io::Result<String> {
    let mut child = Command::new("git")
        .args(["ls-remote", "--tags", repo_url])
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", dir)
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .env("GIT_HTTP_LOW_SPEED_TIME", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("update probe timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut out)?;
    }
    if !child.wait()?.success() {
        return Err(io::Error::other("git ls-remote failed"));
    }
    Ok(out)
}

/// Spawn the check. Sends `Ev::UpdateCheck(Some(version-string))` when a
/// newer release exists — immediately from cache when available, then from a
/// fresh probe if the 24h window has elapsed.
pub fn spawn(tx: Sender<Ev>) {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    std::thread::spawn(move || {
        let current = Version::current();
        let (last_check, cached_latest) = read_cache();
        if let Some(v) = cached_latest.filter(|v| *v > current) {
            let _ = tx.send(Ev::UpdateCheck(Some(v.to_string())));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if !should_check(now, last_check) {
            return;
        }
        let Ok(output) = probe(env!("CARGO_PKG_REPOSITORY")) else {
            return; // fail-silent; cache untouched so next launch retries
        };
        let latest = latest_stable(&output);
        write_cache(now, latest);
        let newer = latest.filter(|v| *v > current).map(|v| v.to_string());
        let _ = tx.send(Ev::UpdateCheck(newer));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_order() {
        assert_eq!(Version::parse("1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("v0.10.0").unwrap().to_string(), "0.10.0");
        assert!(Version::parse("1.2.3-rc1").is_none()); // pre-release ≠ stable
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("x.y.z").is_none());
        assert!(Version::parse("v0.2.0").unwrap() > Version::parse("0.1.9").unwrap());
    }

    #[test]
    fn tag_refs_clean_only_and_latest_wins() {
        let out = "\
aaa\trefs/tags/v0.1.0\n\
bbb\trefs/tags/v0.3.0^{}\n\
ccc\trefs/tags/v0.2.0\n\
ddd\trefs/tags/nightly\n";
        assert_eq!(parse_tag_ref("aaa\trefs/tags/v0.1.0").unwrap().to_string(), "0.1.0");
        assert!(parse_tag_ref("bbb\trefs/tags/v0.3.0^{}").is_none());
        assert_eq!(latest_stable(out).unwrap().to_string(), "0.2.0");
        assert!(latest_stable("junk with no tabs\n").is_none());
    }

    #[test]
    fn check_throttled_to_24h_with_skew_tolerance() {
        let now = 1_700_000_000;
        assert!(should_check(now, 0)); // never checked
        assert!(!should_check(now, now - 900)); // 15 min ago: throttled
        assert!(should_check(now, now - CHECK_INTERVAL_SECS)); // exactly 24h
        assert!(should_check(now, now + 5000)); // future timestamp → check now
    }

    #[test]
    fn current_version_parses() {
        let _ = Version::current(); // panics if Cargo.toml version isn't semver
    }
}
