//! Discord main gateway client for user accounts.

mod event;
mod id;
mod inflate;
mod model;
mod properties;
mod shard;

pub use event::Event;
pub use id::Id;
pub use model::*;
pub use properties::{ClientProperties, FALLBACK_BUILD_NUMBER, FALLBACK_CHROME_MAJOR, chrome_user_agent};
pub use shard::{CAPABILITIES, Config, ConnectionState, Gateway, Notification};
