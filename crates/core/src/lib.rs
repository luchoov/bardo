//! Session state and orchestration. Exposes a UI-agnostic API: start a client, receive events.

mod client;
pub mod permissions;
mod snapshot;
mod store;

pub use bardo_api::MemberChange;
pub use bardo_gateway::{ConnectionState, Id};
pub use client::{Client, ClientEvent, Command};
pub use snapshot::{ChannelView, GuildView, Moderation, Participant, Snapshot};
