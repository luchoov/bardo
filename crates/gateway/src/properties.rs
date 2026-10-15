//! Client identity ("super properties") sent on identify and, base64-encoded, on REST requests.
//!
//! bardo presents itself as the Discord web client running in Chrome on Windows: it is the
//! identity with the fewest moving parts, and both values that drift over time (the client
//! build number and the Chrome version) can be fetched at startup.

use serde::Serialize;
use uuid::Uuid;

/// Used when the build number cannot be scraped from discord.com.
pub const FALLBACK_BUILD_NUMBER: u32 = 637_268;
/// Used when the current stable Chrome version cannot be fetched.
pub const FALLBACK_CHROME_MAJOR: u32 = 156;

#[derive(Debug, Clone, Serialize)]
pub struct ClientProperties {
    pub os: String,
    pub browser: String,
    pub device: String,
    pub system_locale: String,
    pub has_client_mods: bool,
    pub browser_user_agent: String,
    pub browser_version: String,
    pub os_version: String,
    pub referrer: String,
    pub referring_domain: String,
    pub referrer_current: String,
    pub referring_domain_current: String,
    pub release_channel: String,
    pub client_build_number: u32,
    pub client_event_source: Option<String>,
    pub client_launch_id: String,
    pub client_app_state: String,
    pub client_heartbeat_session_id: String,
    pub launch_signature: String,
}

impl ClientProperties {
    pub fn web(chrome_major: u32, build_number: u32) -> Self {
        Self {
            os: "Windows".into(),
            browser: "Chrome".into(),
            device: String::new(),
            system_locale: sys_locale::get_locale().unwrap_or_else(|| "en-US".into()),
            has_client_mods: false,
            browser_user_agent: chrome_user_agent(chrome_major),
            browser_version: format!("{chrome_major}.0.0.0"),
            os_version: "10".into(),
            referrer: String::new(),
            referring_domain: String::new(),
            referrer_current: String::new(),
            referring_domain_current: String::new(),
            release_channel: "stable".into(),
            client_build_number: build_number,
            client_event_source: None,
            client_launch_id: Uuid::new_v4().to_string(),
            client_app_state: "focused".into(),
            client_heartbeat_session_id: Uuid::new_v4().to_string(),
            launch_signature: launch_signature(),
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.browser_user_agent
    }
}

/// Chrome reports a reduced user agent: only the major version is real.
pub fn chrome_user_agent(major: u32) -> String {
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/{major}.0.0.0 Safari/537.36"
    )
}

/// A random UUID with the specific bits the official client clears in its launch signature.
fn launch_signature() -> String {
    const CLEARED_BITS: u128 = 0b00000000100000000001000000010000000010000001000000001000000000000010000010000001000000000100000000000001000000000000100000000000;
    Uuid::from_u128(Uuid::new_v4().as_u128() & !CLEARED_BITS).to_string()
}
