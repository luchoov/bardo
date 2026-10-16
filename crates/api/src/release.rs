//! bardo's own releases on GitHub: whether a newer version is out, and downloading it.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/luchoov/bardo/releases/latest";
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
/// A download may take a while on a slow connection, as long as it keeps moving.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The release assets, as the release workflow names them.
const INSTALLER_ASSET: &str = "bardo-setup.exe";
const PORTABLE_ASSET: &str = "bardo.exe";
const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading "v", e.g. "0.2.0".
    pub version: String,
    /// The release page.
    pub url: String,
    /// Download links by file name.
    assets: Vec<(String, String)>,
}

/// Which file of a release to download: the one that matches how bardo was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    Installer,
    Portable,
}

impl Package {
    fn asset(self) -> &'static str {
        match self {
            Self::Installer => INSTALLER_ASSET,
            Self::Portable => PORTABLE_ASSET,
        }
    }
}

impl Release {
    fn asset_url(&self, name: &str) -> Result<&str, BoxError> {
        let asset = self.assets.iter().find(|(asset, _)| asset == name);
        asset.map(|(_, url)| url.as_str()).ok_or_else(|| format!("the release has no {name}").into())
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The latest published release, if it is newer than `current` (a version like "0.1.0").
pub async fn newer_release(current: &str) -> Result<Option<Release>, BoxError> {
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
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
    let assets = latest.assets.into_iter().map(|asset| (asset.name, asset.browser_download_url)).collect();
    Ok(is_newer(version, current).then(|| Release { version: version.to_owned(), url: latest.html_url, assets }))
}

/// Downloads `package` from `release` and checks it against the release's checksums. `on_progress`
/// gets the fraction done, when the size is known.
pub async fn download(
    release: &Release,
    package: Package,
    mut on_progress: impl FnMut(f32),
) -> Result<Vec<u8>, BoxError> {
    let http = reqwest::Client::builder()
        .connect_timeout(RELEASE_TIMEOUT)
        .read_timeout(DOWNLOAD_READ_TIMEOUT)
        .user_agent(format!("bardo/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let checksums = http.get(release.asset_url(CHECKSUMS_ASSET)?).send().await?.error_for_status()?.text().await?;
    let expected = checksum(&checksums, package.asset()).ok_or("the checksums do not list the download")?;

    let mut response = http.get(release.asset_url(package.asset())?).send().await?.error_for_status()?;
    let size = response.content_length().filter(|&size| size > 0);
    let mut bytes = Vec::with_capacity(size.unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if let Some(size) = size {
            on_progress((bytes.len() as f64 / size as f64).min(1.0) as f32);
        }
    }

    let actual: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != expected {
        return Err(format!("{} does not match its checksum", package.asset()).into());
    }
    Ok(bytes)
}

/// The SHA-256 listed for `name` in a checksums file of `<hex>  <name>` lines.
fn checksum(checksums: &str, name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim_start() == name).then(|| hash.to_ascii_lowercase())
    })
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
    fn finds_checksums_by_file_name() {
        let checksums = "AB12  bardo.exe\r\ncd34  bardo-setup.exe\r\n";
        assert_eq!(checksum(checksums, "bardo.exe").as_deref(), Some("ab12"));
        assert_eq!(checksum(checksums, "bardo-setup.exe").as_deref(), Some("cd34"));
        assert_eq!(checksum(checksums, "SHA256SUMS.txt"), None);
    }

    #[test]
    fn ignores_versions_it_cannot_read() {
        assert!(!is_newer("0.2", "0.1.0"));
        assert!(!is_newer("0.2.0-beta", "0.1.0"));
        assert!(!is_newer("0.2.0.1", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }
}
