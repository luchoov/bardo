//! Voice connections on top of songbird's driver, which handles the voice gateway, UDP/RTP,
//! transport encryption, DAVE end-to-end encryption and Opus.

mod receive;
mod transmit;

use std::{
    num::{NonZeroU8, NonZeroU64},
    sync::{Arc, Mutex},
};

use bardo_audio::{PlaybackSink, SAMPLE_RATE};
use bardo_gateway::Id;
use songbird::{
    Config, CoreEvent, Driver, Event,
    driver::{Channels, DecodeConfig, DecodeMode, SampleRate},
    id::{ChannelId, GuildId, UserId},
    input::RawAdapter,
};
use tracing::info;

pub use crate::transmit::MicFeed;
use crate::receive::{Handler, Receiver};

/// Jitter buffer depth, in 20 ms packets. songbird's default (5) adds 100 ms of latency.
const PLAYOUT_BUFFER: NonZeroU8 = NonZeroU8::new(3).unwrap();

/// What the voice server needs to accept us: our gateway session plus VOICE_SERVER_UPDATE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The guild, or the DM channel for calls.
    pub server_id: Id,
    pub channel_id: Id,
    pub user_id: Id,
    pub session_id: String,
    pub endpoint: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Connected,
    Reconnected,
    /// The connection dropped and songbird gave up; `reason` is `None` when we asked to leave.
    Disconnected { reason: Option<String> },
    /// The users currently heard speaking, sorted.
    Speaking(Vec<Id>),
    /// Our microphone started or stopped being sent.
    Transmitting { user_id: Id, transmitting: bool },
}

pub type EventCallback = Arc<dyn Fn(VoiceEvent) + Send + Sync>;

/// A voice session. Must be created inside a tokio runtime.
pub struct Voice {
    driver: Driver,
    receiver: Arc<Mutex<Receiver>>,
    on_event: EventCallback,
}

impl Voice {
    /// `sink` receives the mixed audio of everyone in the channel.
    pub fn new(sink: Option<PlaybackSink>, on_event: EventCallback) -> Self {
        let config = Config::default()
            .decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Stereo, SampleRate::Hz48000)))
            .playout_buffer_length(PLAYOUT_BUFFER);
        let mut driver = Driver::new(config);

        let receiver = Arc::new(Mutex::new(Receiver::new(sink)));
        let handler = Handler::new(receiver.clone(), on_event.clone());
        for event in [
            CoreEvent::VoiceTick,
            CoreEvent::SpeakingStateUpdate,
            CoreEvent::ClientDisconnect,
            CoreEvent::DriverConnect,
            CoreEvent::DriverReconnect,
            CoreEvent::DriverDisconnect,
        ] {
            driver.add_global_event(Event::Core(event), handler.clone());
        }
        Self { driver, receiver, on_event }
    }

    /// Starts the microphone track, paused. Feed it from the capture thread through the returned
    /// [`MicFeed`], which plays and pauses the track as the transmit gate opens and closes.
    pub fn start_microphone(&mut self, me: Id) -> MicFeed {
        let (producer, source) = transmit::channel();
        let track = self.driver.play_only_input(RawAdapter::new(source, SAMPLE_RATE, 1).into());
        if let Err(e) = track.pause() {
            tracing::warn!("failed to pause the new microphone track: {e}");
        }
        MicFeed::new(producer, track, me, self.on_event.clone())
    }

    /// Replaces where received audio is played, e.g. after switching output devices.
    pub fn set_playback_sink(&self, sink: Option<PlaybackSink>) {
        self.receiver().sink = sink;
    }

    /// Connects, or moves the connection to another channel or server. Resolves once the voice
    /// handshake finishes.
    pub fn connect(&mut self, info: ConnectionInfo) -> impl Future<Output = Result<(), String>> + use<> {
        info!(server = %info.server_id, channel = %info.channel_id, endpoint = %info.endpoint, "connecting to voice");
        let connect = match songbird_info(info) {
            Some(info) => Ok(self.driver.connect(info)),
            None => Err("invalid voice connection ids".to_owned()),
        };
        async move { connect?.await.map_err(|e| e.to_string()) }
    }

    pub fn leave(&mut self) {
        self.driver.leave();
    }

    /// Playback gain for a user: 0.0 silences them, 1.0 is unchanged, up to 2.0.
    pub fn set_user_gain(&self, user_id: Id, gain: f32) {
        self.receiver().set_gain(user_id, gain);
    }

    /// Stops all playback while deafened.
    pub fn set_deafened(&self, deafened: bool) {
        self.receiver().deafened = deafened;
    }

    fn receiver(&self) -> std::sync::MutexGuard<'_, Receiver> {
        self.receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn songbird_info(info: ConnectionInfo) -> Option<songbird::ConnectionInfo> {
    Some(songbird::ConnectionInfo {
        channel_id: ChannelId(NonZeroU64::new(info.channel_id.0)?),
        endpoint: info.endpoint,
        guild_id: GuildId(NonZeroU64::new(info.server_id.0)?),
        session_id: info.session_id,
        token: info.token,
        user_id: UserId(NonZeroU64::new(info.user_id.0)?),
    })
}
