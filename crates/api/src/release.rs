//! bardo's own releases on GitHub, to tell the user when a newer version is out.

use std::time::Duration;

use serde::Deserialize;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/luchoov/bardo/releases/latest";
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading "v", e.g. "0.2.0".
    pub version: String,
    /// The release page.
    pub url: String,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The latest published release, if it is newer than `current` (a version like "0.1.0").
pub async fn newer_release(current: &str) -> Result<Option<Release>, BoxError> {
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
        html_url: String,
    }

    let http = reqwest::Client::builder().timeout(RELEASE_TIMEOUT).build()?;
    let response = http
        .get(LATEST_RELEASE_URL)
        .header("user-agent", format!("bardo/{current}"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    // No release published yet.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body = response.error_for_status()?.text().await?;
    let latest: Latest = serde_json::from_str(&body)?;
    let version = latest.tag_name.trim_start_matches('v');
    Ok(is_newer(version, current).then(|| Release { version: version.to_owned(), url: latest.html_url }))
}

/// Whether `version` comes after `current`. Versions that do not parse as `major.minor.patch` never do.
fn is_newer(version: &str, current: &str) -> bool {
    fn parse(version: &str) -> Option<(u64, u64, u64)> {
        let mut parts = version.split('.').map(|part| part.parse().ok());
        let parsed = (parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(parsed)
    }
    matches!((parse(version), parse(current)), (Some(version), Some(current)) if version > current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn ignores_versions_it_cannot_read() {
        assert!(!is_newer("0.2", "0.1.0"));
        assert!(!is_newer("0.2.0-beta", "0.1.0"));
        assert!(!is_newer("0.2.0.1", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }
}
