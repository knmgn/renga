//! Background release-version check.
//!
//! This fork ships through GitHub Releases only — there is no npm
//! package to ask — so the check reads the latest release tag from the
//! GitHub API instead of the npm registry. The tag is `vX.Y.Z`, and
//! [`is_newer`] already strips a leading `v`, so the comparison is
//! unchanged.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared state for the latest version fetched from GitHub Releases.
#[derive(Clone, Default)]
pub struct VersionInfo {
    inner: Arc<Mutex<Option<String>>>,
}

impl VersionInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the latest version if a newer one is available.
    pub fn update_available(&self) -> Option<String> {
        let latest = self.inner.lock().ok()?.clone()?;
        if is_newer(&latest, CURRENT_VERSION) {
            Some(latest)
        } else {
            None
        }
    }

    fn set(&self, version: String) {
        if let Ok(mut lock) = self.inner.lock() {
            *lock = Some(version);
        }
    }
}

/// Spawn a background thread to check GitHub Releases for a newer version.
pub fn spawn_check(info: VersionInfo) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(1)); // delay so it doesn't compete with startup
        if let Ok(version) = fetch_latest() {
            info.set(version);
        }
    });
}

/// Where the update check looks. Kept next to the call so a fork of
/// this fork has one line to change.
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/knmgn/renga/releases/latest";

fn fetch_latest() -> Result<String, Box<dyn std::error::Error>> {
    let response = ureq::get(LATEST_RELEASE_URL)
        // The GitHub API rejects requests without a User-Agent, and
        // pins the response shape to a version when asked.
        .set(
            "User-Agent",
            concat!("renga-cp/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(5))
        .call()?;
    let json: serde_json::Value = response.into_json()?;
    let version = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("no tag_name field")?
        .to_string();
    Ok(version)
}

/// Compare semver-like versions (simple major.minor.patch).
/// A leading `v` (GitHub release tags carry one) and any prerelease
/// suffix (e.g. `-rc.1`) are stripped before comparison, so a
/// prerelease is never surfaced as an update.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.0", "0.3.0"));
    }

    /// GitHub release tags are `vX.Y.Z`, unlike the bare npm versions
    /// this used to read, so the `v` strip is load-bearing now rather
    /// than defensive.
    #[test]
    fn test_is_newer_with_release_tag_prefix() {
        assert!(is_newer("v3.1.0", "3.0.0"));
        assert!(!is_newer("v3.0.0", "3.0.0"));
        assert!(!is_newer("v2.9.9", "3.0.0"));
    }

    #[test]
    fn test_is_newer_with_prerelease() {
        // Prerelease suffix is ignored; only major.minor.patch is compared.
        assert!(!is_newer("0.5.5-fork.1", "0.5.5"));
        assert!(!is_newer("0.5.5", "0.5.5-fork.1"));
        assert!(is_newer("0.6.0-fork.1", "0.5.9"));
        assert!(!is_newer("0.5.4-fork.1", "0.5.5"));
    }
}
