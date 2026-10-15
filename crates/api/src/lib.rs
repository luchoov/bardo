//! Discord REST API and the HTTP requests needed before connecting.

use std::time::Duration;

use bardo_gateway::{ClientProperties, FALLBACK_BUILD_NUMBER, FALLBACK_CHROME_MAJOR, chrome_user_agent};
use serde::Deserialize;
use tracing::{info, warn};

const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
const CHROME_VERSIONS_URL: &str =
    "https://versionhistory.googleapis.com/v1/chrome/platforms/win/channels/stable/versions";
const DISCORD_LOGIN_URL: &str = "https://discord.com/login";

/// Builds the client identity with the current Chrome version and Discord build number,
/// falling back to known-good values if either lookup fails.
pub async fn client_properties() -> ClientProperties {
    let http = match reqwest::Client::builder().timeout(BOOTSTRAP_TIMEOUT).build() {
        Ok(http) => http,
        Err(e) => {
            warn!("failed to build HTTP client: {e}");
            return ClientProperties::web(FALLBACK_CHROME_MAJOR, FALLBACK_BUILD_NUMBER);
        }
    };

    let chrome_major = chrome_major_version(&http).await.unwrap_or_else(|e| {
        warn!("could not fetch Chrome version, using fallback: {e}");
        FALLBACK_CHROME_MAJOR
    });
    let build_number = client_build_number(&http, chrome_major).await.unwrap_or_else(|e| {
        warn!("could not fetch Discord build number, using fallback: {e}");
        FALLBACK_BUILD_NUMBER
    });
    info!(chrome_major, build_number, "client properties ready");

    ClientProperties::web(chrome_major, build_number)
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn chrome_major_version(http: &reqwest::Client) -> Result<u32, BoxError> {
    #[derive(Deserialize)]
    struct Versions {
        versions: Vec<Version>,
    }
    #[derive(Deserialize)]
    struct Version {
        version: String,
    }

    let body = http.get(CHROME_VERSIONS_URL).send().await?.error_for_status()?.text().await?;
    let versions: Versions = serde_json::from_str(&body)?;
    let latest = versions.versions.first().ok_or("empty version list")?;
    let major = latest.version.split('.').next().ok_or("malformed version")?;
    Ok(major.parse()?)
}

/// The web app embeds its build number in the page's global environment.
async fn client_build_number(http: &reqwest::Client, chrome_major: u32) -> Result<u32, BoxError> {
    const MARKER: &str = "\"BUILD_NUMBER\":\"";

    let page = http
        .get(DISCORD_LOGIN_URL)
        .header(reqwest::header::USER_AGENT, chrome_user_agent(chrome_major))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let start = page.find(MARKER).ok_or("build number not found")? + MARKER.len();
    let digits: String = page[start..].chars().take_while(char::is_ascii_digit).collect();
    Ok(digits.parse()?)
}
