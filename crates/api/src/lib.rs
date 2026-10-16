//! Discord REST API: the requests needed before connecting, and an authenticated client that
//! presents itself like the web client the gateway identifies as.

mod bootstrap;
mod error;
mod http;
mod ratelimit;
mod release;

pub use bootstrap::client_properties;
pub use error::Error;
pub use http::{Http, MemberChange};
pub use release::{Release, newer_release};
