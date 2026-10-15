use std::{fmt, time::Duration};

use serde::Deserialize;

#[derive(Debug)]
pub enum Error {
    /// The request could not be sent or its response could not be read.
    Request(reqwest::Error),
    /// Discord rejected the request with a JSON error body.
    Discord { status: u16, code: u64, message: String },
    /// Discord rejected the request without a readable error body.
    Status(u16),
    /// A rate limit too long to wait out.
    RateLimited(Duration),
    /// The token cannot be used as a header value.
    InvalidToken,
}

impl Error {
    pub(crate) fn from_response(status: u16, body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Body {
            code: u64,
            message: String,
        }
        match serde_json::from_slice::<Body>(body) {
            Ok(body) => Error::Discord { status, code: body.code, message: body.message },
            Err(_) => Error::Status(status),
        }
    }

    /// The HTTP status code, if Discord answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Discord { status, .. } | Error::Status(status) => Some(*status),
            Error::RateLimited(_) => Some(429),
            Error::Request(_) | Error::InvalidToken => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Request(e) => write!(f, "request failed: {e}"),
            Error::Discord { code, message, .. } => write!(f, "{message} ({code})"),
            Error::Status(status) => write!(f, "HTTP {status}"),
            Error::RateLimited(retry) => write!(f, "rate limited for {:.0} s", retry.as_secs_f64().ceil()),
            Error::InvalidToken => write!(f, "invalid token"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Request(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Request(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discord_error_bodies() {
        let error = Error::from_response(403, br#"{"message": "Missing Permissions", "code": 50013}"#);
        assert_eq!(error.to_string(), "Missing Permissions (50013)");
        assert_eq!(error.status(), Some(403));

        let error = Error::from_response(502, b"<html>Bad Gateway</html>");
        assert_eq!(error.to_string(), "HTTP 502");
    }
}
