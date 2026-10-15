//! Session state and orchestration. Exposes a UI-agnostic API: start a client, send it commands,
//! receive events.

mod client;
pub mod permissions;
mod snapshot;
mod store;
mod voice;

pub use bardo_api::MemberChange;
pub use bardo_gateway::{ConnectionState, Id};
pub use client::{Client, ClientEvent, Command};
pub use snapshot::{
    ChannelView, FriendInVoice, GuildView, Moderation, Participant, Snapshot, VoiceStatus, VoiceView,
};
pub use bardo_audio::{AudioDevice, DEFAULT_THRESHOLD_DB, SILENCE_DB, TransmitControl, input_devices, output_devices};
pub use voice::{AudioDevices, UserAudio};
