//! Session state and orchestration. Exposes a UI-agnostic API: start a client, send it commands,
//! receive events.

mod client;
pub mod cues;
mod mic_test;
pub mod permissions;
mod snapshot;
mod store;
mod voice;

pub use bardo_api::MemberChange;
pub use bardo_gateway::{ConnectionState, Id};
pub use client::{Client, ClientEvent, Command};
pub use mic_test::MicTest;
pub use snapshot::{
    CallView, ChannelView, FriendInVoice, GuildView, Moderation, Participant, PrivateChannelView, Snapshot,
    VoiceStatus, VoiceView,
};
pub use bardo_audio::{
    AudioDevice, DEFAULT_THRESHOLD_DB, Ringer, SILENCE_DB, Sound, TransmitControl, input_devices, output_devices,
    play_sound,
};
pub use voice::{AudioDevices, UserAudio};
