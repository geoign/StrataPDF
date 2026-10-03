//! Update check against the releases on GitHub: at start-up unless turned off in
//! the settings, and from the help menu. The latest release is found without the
//! API (which is rate-limited): `releases/latest` answers with a redirect to
//! `releases/tag/vX.Y.Z`, and the tag is the version.

use std::time::Duration;

use crossbeam_channel::Sender;
use strata_core::Waker;

/// The page of the latest release.
pub const RELEASES_URL: &str = "https://github.com/geoign/StrataPDF/releases/latest";

/// This build's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// "0.2.4"
    pub version: String,
    /// The release page.
    pub url: String,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Newer(Release),
    UpToDate(String),
    Failed(String),
}

/// Run the check on a thread: the result goes to `tx`, and `waker` repaints.
pub fn spawn(tx: Sender<Outcome>, waker: Waker) {
    let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
        let o = check();
        let _ = tx.send(o);
        waker();
    });
}

pub fn check() -> Outcome {
    match latest() {
        Ok(r) if newer(&r.version, CURRENT) => Outcome::Newer(r),
        Ok(r) => Outcome::UpToDate(r.version),
        Err(e) => Outcome::Failed(e),
    }
}

fn latest() -> Result<Release, String> {
    let config = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(Duration::from_secs(10))).build();
    let agent: ureq::Agent = config.into();
    let resp = agent.get(RELEASES_URL).header("User-Agent", &format!("StrataPDF/{CURRENT}")).call().map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if !(300..400).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or("no redirect to the latest release")?.to_string();
    let tag = loc.rsplit('/').next().unwrap_or("");
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if parse(&version).is_none() {
        return Err(format!("unexpected tag {tag}"));
    }
    let url = if loc.starts_with("http") { loc.clone() } else { format!("https://github.com{loc}") };
    Ok(Release { version, url })
}

/// "0.2.4", "v0.2.4" or "0.2.4-beta" as (0, 2, 4).
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map(|p| p.parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next()??;
    let patch = it.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether `latest` is a later version than `current`.
pub fn newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the network: `cargo test -p strata-app update:: -- --ignored`.
    #[test]
    #[ignore]
    fn latest_release_is_reachable() {
        let r = latest().expect("latest release");
        assert!(parse(&r.version).is_some(), "{r:?}");
        assert!(r.url.starts_with("https://github.com/geoign/StrataPDF/releases/tag/v"), "{r:?}");
        eprintln!("latest: {r:?}, current {CURRENT}, newer: {}", newer(&r.version, CURRENT));
    }

    #[test]
    fn versions_compare() {
        assert!(newer("0.2.4", "0.2.3"));
        assert!(newer("v0.3.0", "0.2.9"));
        assert!(newer("1.0.0", "0.10.5"));
        assert!(!newer("0.2.3", "0.2.3"));
        assert!(!newer("v0.2.2", "0.2.3"));
        assert!(!newer("latest", "0.2.3"));
        assert!(newer("0.2.4-beta", "0.2.3"));
        assert_eq!(parse("v0.2.3"), Some((0, 2, 3)));
        assert_eq!(parse("0.2"), Some((0, 2, 0)));
    }
}
