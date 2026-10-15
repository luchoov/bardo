//! REST rate limits. Discord groups routes into buckets that it names in the `X-RateLimit-Bucket`
//! response header; until a route's bucket is known, the route itself is the key. Either way the
//! key is scoped by the route's major parameter (here, always a guild or channel id).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use reqwest::header::HeaderMap;
use serde::Deserialize;
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{self, Instant},
};

/// Requests that share a bucket are sent one at a time, so its remaining count is never raced.
pub(crate) type BucketLock = Arc<AsyncMutex<Bucket>>;

#[derive(Default)]
pub(crate) struct RateLimiter {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Route → bucket hash reported by Discord.
    hashes: HashMap<&'static str, String>,
    buckets: HashMap<String, BucketLock>,
    global_until: Option<Instant>,
}

impl RateLimiter {
    pub fn bucket(&self, route: &'static str, major: u64) -> BucketLock {
        let mut state = self.state();
        let key = match state.hashes.get(route) {
            Some(hash) => format!("{hash}:{major}"),
            None => format!("{route}:{major}"),
        };
        state.buckets.entry(key).or_default().clone()
    }

    /// Records the bucket Discord reported for a route; later requests on the route share it.
    pub fn learn(&self, route: &'static str, major: u64, hash: &str, bucket: &BucketLock) {
        let mut state = self.state();
        if state.hashes.get(route).is_some_and(|known| known == hash) {
            return;
        }
        state.hashes.insert(route, hash.to_owned());
        state.buckets.entry(format!("{hash}:{major}")).or_insert_with(|| bucket.clone());
    }

    pub async fn wait_global(&self) {
        loop {
            let until = self.state().global_until;
            match until {
                Some(until) if until > Instant::now() => time::sleep_until(until).await,
                _ => return,
            }
        }
    }

    pub fn block_global(&self, retry_after: Duration) {
        let until = Instant::now() + retry_after;
        let mut state = self.state();
        state.global_until = Some(state.global_until.map_or(until, |current| current.max(until)));
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Default)]
pub(crate) struct Bucket {
    remaining: Option<u32>,
    reset_at: Option<Instant>,
}

impl Bucket {
    /// How long to wait before another request fits in the bucket.
    pub fn delay(&self, now: Instant) -> Option<Duration> {
        match (self.remaining, self.reset_at) {
            (Some(0), Some(reset_at)) if reset_at > now => Some(reset_at - now),
            _ => None,
        }
    }

    pub fn update(&mut self, limits: &Limits, now: Instant) {
        if let Some(remaining) = limits.remaining {
            self.remaining = Some(remaining);
        }
        if let Some(reset_after) = limits.reset_after {
            self.reset_at = Some(now + reset_after);
        }
    }

    pub fn block(&mut self, retry_after: Duration, now: Instant) {
        self.remaining = Some(0);
        self.reset_at = Some(now + retry_after);
    }
}

/// The rate limit headers of a response.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Limits {
    pub bucket: Option<String>,
    pub remaining: Option<u32>,
    pub reset_after: Option<Duration>,
}

impl Limits {
    pub fn parse(headers: &HeaderMap) -> Self {
        Self {
            bucket: header(headers, "x-ratelimit-bucket").map(str::to_owned),
            remaining: header(headers, "x-ratelimit-remaining").and_then(|v| v.parse().ok()),
            reset_after: header(headers, "x-ratelimit-reset-after").and_then(seconds),
        }
    }
}

/// How long a 429 response asks us to wait, and whether the limit applies to every route.
pub(crate) fn retry_after(headers: &HeaderMap, body: &[u8]) -> (Duration, bool) {
    #[derive(Deserialize)]
    struct Body {
        retry_after: f64,
        #[serde(default)]
        global: bool,
    }

    let header_global = header(headers, "x-ratelimit-global") == Some("true")
        || header(headers, "x-ratelimit-scope") == Some("global");
    match serde_json::from_slice::<Body>(body) {
        Ok(body) => (
            Duration::try_from_secs_f64(body.retry_after).unwrap_or(FALLBACK_RETRY),
            body.global || header_global,
        ),
        // Without a JSON body the 429 came from Cloudflare, which limits by IP: treat it as global.
        Err(_) => (header(headers, "retry-after").and_then(seconds).unwrap_or(FALLBACK_RETRY), true),
    }
}

const FALLBACK_RETRY: Duration = Duration::from_secs(5);

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn seconds(value: &str) -> Option<Duration> {
    value.parse::<f64>().ok().and_then(|s| Duration::try_from_secs_f64(s).ok())
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn exhausted_bucket_waits_until_reset() {
        let now = Instant::now();
        let mut bucket = Bucket::default();
        assert_eq!(bucket.delay(now), None);

        let limits = Limits::parse(&headers(&[
            ("x-ratelimit-bucket", "abc"),
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset-after", "1.5"),
        ]));
        assert_eq!(limits.bucket.as_deref(), Some("abc"));
        bucket.update(&limits, now);
        assert_eq!(bucket.delay(now), Some(Duration::from_millis(1500)));
        assert_eq!(bucket.delay(now + Duration::from_secs(2)), None);
    }

    #[test]
    fn remaining_requests_do_not_wait() {
        let now = Instant::now();
        let mut bucket = Bucket::default();
        bucket.update(
            &Limits::parse(&headers(&[("x-ratelimit-remaining", "3"), ("x-ratelimit-reset-after", "10")])),
            now,
        );
        assert_eq!(bucket.delay(now), None);
    }

    #[test]
    fn routes_share_the_bucket_discord_reports() {
        let limiter = RateLimiter::default();
        let first = limiter.bucket("PATCH /guilds/{}/members/{}", 1);
        limiter.learn("PATCH /guilds/{}/members/{}", 1, "hash", &first);
        let second = limiter.bucket("PATCH /guilds/{}/members/{}", 1);
        assert!(Arc::ptr_eq(&first, &second));
        let other_guild = limiter.bucket("PATCH /guilds/{}/members/{}", 2);
        assert!(!Arc::ptr_eq(&first, &other_guild));
    }

    #[test]
    fn parses_429_bodies() {
        let (retry, global) = retry_after(&HeaderMap::new(), br#"{"message": "", "retry_after": 0.25, "global": false}"#);
        assert_eq!((retry, global), (Duration::from_millis(250), false));

        let (retry, global) = retry_after(&headers(&[("retry-after", "7")]), b"error code: 1015");
        assert_eq!((retry, global), (Duration::from_secs(7), true));
    }
}
