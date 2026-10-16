//! Authenticated REST client. Every request carries the same identity the gateway connection
//! uses: the web client's user agent, client hints and super properties.

use std::{
    sync::{RwLock, RwLockReadGuard},
    time::Duration,
};

use bardo_gateway::{ClientProperties, Id};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{
    Method, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, REFERER},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{self, Instant};
use tracing::{debug, warn};

use crate::{
    error::Error,
    ratelimit::{self, Limits, RateLimiter},
};

const API_BASE: &str = "https://discord.com/api/v9";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ATTEMPTS: u32 = 3;
/// Rate limits longer than this are reported to the caller instead of waited out.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
const SERVER_ERROR_RETRY: Duration = Duration::from_secs(1);

pub struct Http {
    client: reqwest::Client,
    /// Headers shared by every request, except the token.
    headers: HeaderMap,
    token: RwLock<HeaderValue>,
    limiter: RateLimiter,
}

/// A change a moderator makes to a guild member's voice state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberChange {
    ServerMute(bool),
    ServerDeafen(bool),
    /// Move to another voice channel, or disconnect from voice with `None`.
    Move(Option<Id>),
}

impl MemberChange {
    fn body(self) -> Value {
        match self {
            MemberChange::ServerMute(mute) => json!({ "mute": mute }),
            MemberChange::ServerDeafen(deaf) => json!({ "deaf": deaf }),
            MemberChange::Move(channel_id) => json!({ "channel_id": channel_id }),
        }
    }
}

struct Route {
    method: Method,
    path: String,
    /// The route with its parameters left out, used as the rate limit key until Discord names the bucket.
    key: &'static str,
    major: Id,
}

impl Http {
    pub fn new(token: &str, properties: &ClientProperties) -> Result<Self, Error> {
        let client = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()?;
        Ok(Self {
            client,
            headers: web_headers(properties),
            token: RwLock::new(token_header(token)?),
            limiter: RateLimiter::default(),
        })
    }

    /// Replaces the token after Discord rotates it.
    pub fn set_token(&self, token: &str) -> Result<(), Error> {
        let header = token_header(token)?;
        *self.token.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = header;
        Ok(())
    }

    /// `PATCH /guilds/{guild}/members/{user}`: server mute, server deafen, move or disconnect.
    pub async fn modify_member(&self, guild_id: Id, user_id: Id, change: MemberChange) -> Result<(), Error> {
        let route = Route {
            method: Method::PATCH,
            path: format!("/guilds/{guild_id}/members/{user_id}"),
            key: "PATCH /guilds/{guild_id}/members/{user_id}",
            major: guild_id,
        };
        self.request(&route, Some(&change.body())).await.map(drop)
    }

    /// `POST /channels/{channel}/call/ring`: rings `recipients`, or everyone in the DM or group.
    pub async fn ring(&self, channel_id: Id, recipients: Option<&[Id]>) -> Result<(), Error> {
        let route = Route {
            method: Method::POST,
            path: format!("/channels/{channel_id}/call/ring"),
            key: "POST /channels/{channel_id}/call/ring",
            major: channel_id,
        };
        self.request(&route, Some(&json!({ "recipients": recipients }))).await.map(drop)
    }

    /// `POST /channels/{channel}/call/stop-ringing`: stops ringing `recipients`; for ourselves,
    /// that declines the call.
    pub async fn stop_ringing(&self, channel_id: Id, recipients: &[Id]) -> Result<(), Error> {
        let route = Route {
            method: Method::POST,
            path: format!("/channels/{channel_id}/call/stop-ringing"),
            key: "POST /channels/{channel_id}/call/stop-ringing",
            major: channel_id,
        };
        self.request(&route, Some(&json!({ "recipients": recipients }))).await.map(drop)
    }

    async fn request(&self, route: &Route, body: Option<&Value>) -> Result<Vec<u8>, Error> {
        let bucket_lock = self.limiter.bucket(route.key, route.major.0);
        let mut bucket = bucket_lock.lock().await;

        let mut last_error = None;
        for attempt in 1..=MAX_ATTEMPTS {
            self.limiter.wait_global().await;
            if let Some(delay) = bucket.delay(Instant::now()) {
                if delay > MAX_RATE_LIMIT_WAIT {
                    return Err(Error::RateLimited(delay));
                }
                debug!(route = route.key, ?delay, "waiting for rate limit bucket");
                time::sleep(delay).await;
            }

            let response = self.client.execute(self.build(route, body)?).await?;
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response.bytes().await?;
            let now = Instant::now();

            let limits = Limits::parse(&headers);
            if let Some(hash) = &limits.bucket {
                self.limiter.learn(route.key, route.major.0, hash, &bucket_lock);
            }
            bucket.update(&limits, now);

            if status == StatusCode::TOO_MANY_REQUESTS {
                let (retry_after, global) = ratelimit::retry_after(&headers, &bytes);
                warn!(route = route.key, ?retry_after, global, attempt, "rate limited");
                if retry_after > MAX_RATE_LIMIT_WAIT {
                    return Err(Error::RateLimited(retry_after));
                }
                if global {
                    self.limiter.block_global(retry_after);
                } else {
                    bucket.block(retry_after, now);
                }
                last_error = Some(Error::RateLimited(retry_after));
                continue;
            }
            if status.is_server_error() && attempt < MAX_ATTEMPTS {
                warn!(route = route.key, %status, attempt, "server error, retrying");
                last_error = Some(Error::Status(status.as_u16()));
                time::sleep(SERVER_ERROR_RETRY).await;
                continue;
            }
            if status.is_success() {
                return Ok(bytes.to_vec());
            }
            return Err(Error::from_response(status.as_u16(), &bytes));
        }
        Err(last_error.unwrap_or(Error::Status(StatusCode::TOO_MANY_REQUESTS.as_u16())))
    }

    fn build(&self, route: &Route, body: Option<&Value>) -> Result<reqwest::Request, Error> {
        let mut builder = self
            .client
            .request(route.method.clone(), format!("{API_BASE}{}", route.path))
            .headers(self.headers.clone())
            .header(AUTHORIZATION, self.token().clone());
        if let Some(body) = body {
            builder = builder.header(CONTENT_TYPE, "application/json").body(body.to_string());
        }
        Ok(builder.build()?)
    }

    fn token(&self) -> RwLockReadGuard<'_, HeaderValue> {
        self.token.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `POST /users/@me/remote-auth/login`: exchanges the ticket of a QR code sign-in for the token,
/// encrypted for that session's key. Sent without a token, like from the web client's sign-in page.
pub(crate) async fn remote_auth_login(properties: &ClientProperties, ticket: &str) -> Result<String, Error> {
    #[derive(Deserialize)]
    struct Body {
        encrypted_token: String,
    }

    let client = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()?;
    let mut headers = web_headers(properties);
    headers.insert(REFERER, HeaderValue::from_static("https://discord.com/login"));
    let response = client
        .post(format!("{API_BASE}/users/@me/remote-auth/login"))
        .headers(headers)
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "ticket": ticket }).to_string())
        .send()
        .await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;
    if !(200..300).contains(&status) {
        return Err(Error::from_response(status, &bytes));
    }
    serde_json::from_slice::<Body>(&bytes).map(|body| body.encrypted_token).map_err(|_| Error::Status(status))
}

fn token_header(token: &str) -> Result<HeaderValue, Error> {
    let mut header = HeaderValue::from_str(token).map_err(|_| Error::InvalidToken)?;
    header.set_sensitive(true);
    Ok(header)
}

/// The headers Chrome sends on the web client's API requests.
fn web_headers(properties: &ClientProperties) -> HeaderMap {
    let chrome_major: u32 = properties
        .browser_version
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(bardo_gateway::FALLBACK_CHROME_MAJOR);
    let super_properties = BASE64.encode(serde_json::to_vec(properties).unwrap_or_default());
    let timezone = iana_time_zone::get_timezone().ok();

    let pairs = [
        ("accept", Some("*/*".to_owned())),
        ("accept-language", Some(accept_language(&properties.system_locale))),
        ("origin", Some("https://discord.com".to_owned())),
        ("priority", Some("u=1, i".to_owned())),
        ("referer", Some("https://discord.com/channels/@me".to_owned())),
        ("sec-ch-ua", Some(sec_ch_ua(chrome_major))),
        ("sec-ch-ua-mobile", Some("?0".to_owned())),
        ("sec-ch-ua-platform", Some("\"Windows\"".to_owned())),
        ("sec-fetch-dest", Some("empty".to_owned())),
        ("sec-fetch-mode", Some("cors".to_owned())),
        ("sec-fetch-site", Some("same-origin".to_owned())),
        ("user-agent", Some(properties.user_agent().to_owned())),
        ("x-debug-options", Some("bugReporterEnabled".to_owned())),
        ("x-discord-locale", Some(discord_locale(&properties.system_locale).to_owned())),
        ("x-discord-timezone", timezone),
        ("x-super-properties", Some(super_properties)),
    ];

    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let Some(value) = value.and_then(|v| HeaderValue::from_str(&v).ok()) else { continue };
        headers.insert(HeaderName::from_static(name), value);
    }
    headers
}

/// Chromium's `Sec-CH-UA` brand list: a GREASE brand plus Chromium and Google Chrome, shuffled
/// deterministically by the major version.
fn sec_ch_ua(major: u32) -> String {
    const GREASE_CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];

    let seed = major as usize;
    let grease = format!(
        "Not{}A{}Brand",
        GREASE_CHARS[seed % GREASE_CHARS.len()],
        GREASE_CHARS[(seed + 1) % GREASE_CHARS.len()]
    );
    let brands = [
        (grease, GREASE_VERSIONS[seed % GREASE_VERSIONS.len()].to_owned()),
        ("Chromium".to_owned(), major.to_string()),
        ("Google Chrome".to_owned(), major.to_string()),
    ];
    let mut shuffled: [Option<(String, String)>; 3] = Default::default();
    for (brand, &slot) in brands.into_iter().zip(&ORDERS[seed % ORDERS.len()]) {
        shuffled[slot] = Some(brand);
    }
    shuffled
        .into_iter()
        .flatten()
        .map(|(brand, version)| format!("\"{brand}\";v=\"{version}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn accept_language(locale: &str) -> String {
    let language = locale.split(['-', '_']).next().unwrap_or("en");
    let mut value = locale.replace('_', "-");
    if language != value {
        value.push_str(&format!(",{language};q=0.9"));
    }
    if language != "en" {
        value.push_str(",en;q=0.8");
    }
    value
}

/// Maps the system locale to the closest locale the Discord client offers.
fn discord_locale(locale: &str) -> &'static str {
    const PLAIN: [&str; 24] = [
        "bg", "cs", "da", "de", "el", "fi", "fr", "hi", "hr", "hu", "id", "it", "ja", "ko", "lt", "nl", "no",
        "pl", "ro", "ru", "th", "tr", "uk", "vi",
    ];
    let mut parts = locale.split(['-', '_']);
    let language = parts.next().unwrap_or_default();
    let region = parts.next_back().unwrap_or_default();
    match (language, region) {
        ("en", "GB") => "en-GB",
        ("es", "ES") => "es-ES",
        ("es", _) => "es-419",
        ("pt", _) => "pt-BR",
        ("sv", _) => "sv-SE",
        ("zh", "TW" | "HK" | "MO" | "Hant") => "zh-TW",
        ("zh", _) => "zh-CN",
        ("nb" | "nn", _) => "no",
        (language, _) => PLAIN.iter().find(|&&plain| plain == language).copied().unwrap_or("en-US"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http() -> Http {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut properties = ClientProperties::web(120, 400_000);
        properties.system_locale = "es-AR".into();
        Http::new("secret-token", &properties).unwrap()
    }

    fn modify_member_request(http: &Http, change: MemberChange) -> reqwest::Request {
        let route = Route {
            method: Method::PATCH,
            path: format!("/guilds/{}/members/{}", Id(10), Id(20)),
            key: "PATCH /guilds/{guild_id}/members/{user_id}",
            major: Id(10),
        };
        http.build(&route, Some(&change.body())).unwrap()
    }

    fn body(request: &reqwest::Request) -> Value {
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[test]
    fn member_changes_patch_the_member() {
        let http = http();
        let request = modify_member_request(&http, MemberChange::ServerMute(true));
        assert_eq!(request.method(), Method::PATCH);
        assert_eq!(request.url().as_str(), "https://discord.com/api/v9/guilds/10/members/20");
        assert_eq!(request.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(body(&request), json!({ "mute": true }));

        let request = modify_member_request(&http, MemberChange::ServerDeafen(false));
        assert_eq!(body(&request), json!({ "deaf": false }));
        let request = modify_member_request(&http, MemberChange::Move(Some(Id(30))));
        assert_eq!(body(&request), json!({ "channel_id": "30" }));
        let request = modify_member_request(&http, MemberChange::Move(None));
        assert_eq!(body(&request), json!({ "channel_id": null }));
    }

    #[test]
    fn requests_carry_the_web_client_identity() {
        let http = http();
        let request = modify_member_request(&http, MemberChange::ServerMute(true));
        let headers = request.headers();
        assert_eq!(headers[AUTHORIZATION], "secret-token");
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert_eq!(headers["user-agent"], bardo_gateway::chrome_user_agent(120));
        assert_eq!(headers["x-discord-locale"], "es-419");
        assert_eq!(headers["accept-language"], "es-AR,es;q=0.9,en;q=0.8");

        let decoded = BASE64.decode(headers["x-super-properties"].as_bytes()).unwrap();
        let properties: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(properties["client_build_number"], 400_000);
        assert_eq!(properties["browser_version"], "120.0.0.0");

        http.set_token("rotated").unwrap();
        let request = modify_member_request(&http, MemberChange::ServerMute(true));
        assert_eq!(request.headers()[AUTHORIZATION], "rotated");
    }

    #[test]
    fn client_hints_match_chromium() {
        assert_eq!(sec_ch_ua(120), r#""Not_A Brand";v="8", "Chromium";v="120", "Google Chrome";v="120""#);
    }

    #[test]
    fn maps_system_locales() {
        assert_eq!(discord_locale("es-AR"), "es-419");
        assert_eq!(discord_locale("es-ES"), "es-ES");
        assert_eq!(discord_locale("en-AU"), "en-US");
        assert_eq!(discord_locale("de-DE"), "de");
        assert_eq!(discord_locale("zh-Hant-TW"), "zh-TW");
        assert_eq!(discord_locale("xx"), "en-US");
        assert_eq!(accept_language("en-US"), "en-US,en;q=0.9");
    }
}
