//! Token storage in Windows Credential Manager. `BARDO_TOKEN` overrides it during development.

use keyring_core::{Entry, Error};
use tracing::warn;

const SERVICE: &str = "bardo";
const ACCOUNT: &str = "discord-token";

pub fn init() {
    match windows_native_keyring_store::Store::new() {
        Ok(store) => keyring_core::set_default_store(store),
        Err(e) => warn!("credential store unavailable: {e}"),
    }
}

pub fn load() -> Option<String> {
    if let Some(token) = std::env::var("BARDO_TOKEN").ok().and_then(|t| normalize(&t)) {
        return Some(token);
    }
    match Entry::new(SERVICE, ACCOUNT).and_then(|entry| entry.get_password()) {
        Ok(token) => Some(token),
        Err(Error::NoEntry) => None,
        Err(e) => {
            warn!("failed to read stored token: {e}");
            None
        }
    }
}

pub fn save(token: &str) -> Result<(), Error> {
    Entry::new(SERVICE, ACCOUNT)?.set_password(token)
}

pub fn clear() {
    match Entry::new(SERVICE, ACCOUNT).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(Error::NoEntry) => {}
        Err(e) => warn!("failed to delete stored token: {e}"),
    }
}

/// Accepts a token pasted with surrounding whitespace or quotes.
pub fn normalize(input: &str) -> Option<String> {
    let token = input.trim().trim_matches(|c| c == '"' || c == '\'');
    (!token.is_empty() && !token.contains(char::is_whitespace)).then(|| token.to_owned())
}
