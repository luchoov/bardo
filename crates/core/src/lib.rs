//! Session state and orchestration. Exposes a UI-agnostic API: start a client, send it commands,
//! receive events.

mod cdn;
mod client;
pub mod cues;
mod mic_test;
pub mod permissions;
mod qr_login;
mod snapshot;
mod store;
mod update;
mod voice;

pub use bardo_api::{MemberChange, Package, Release};
pub use bardo_gateway::{ConnectionState, Id};
pub use cdn::CdnFetcher;
pub use client::{Client, ClientEvent, Command};
pub use mic_test::{MicTest, MicTestKind, MicTestPhase};
pub use qr_login::{QrLogin, QrLoginEvent};
pub use snapshot::{
    CallView, CdnImage, ChannelView, FriendInVoice, GuildView, Moderation, Participant, PrivateChannelView, Snapshot,
    VoiceStatus, VoiceView,
};
pub use bardo_audio::{
    AudioDevice, DEFAULT_THRESHOLD_DB, Ringer, SILENCE_DB, Sound, TransmitControl, input_devices, output_devices,
    play_sound,
};
pub use update::{UpdateEvent, download_update};
pub use voice::{AudioDevices, UserAudio};
