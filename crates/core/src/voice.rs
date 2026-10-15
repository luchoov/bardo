//! Our own voice connection. Joining takes three steps: op 4 asks the gateway to put us in a
//! channel, the gateway answers with our VOICE_STATE_UPDATE (voice session id) and a
//! VOICE_SERVER_UPDATE (endpoint and token), and only then can the voice driver connect.
//!
//! [`VoiceSession`] is the state machine; it emits [`Action`]s that [`VoiceDriver`] carries out.

use std::{collections::HashMap, mem, sync::Arc};

use bardo_audio::{Capture, Playback, TransmitControl, Transmitter};
use bardo_gateway::{Gateway, Id, UpdateVoiceState, VoiceServerUpdate, VoiceState};
use bardo_voice::{ConnectionInfo, EventCallback, Voice, VoiceEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    client::{ClientEvent, Internal},
    snapshot::{VoiceStatus, VoiceView},
    store::Store,
};

/// Playback settings for one user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserAudio {
    /// Percent, 0–200.
    pub volume: u16,
    pub muted: bool,
}

impl UserAudio {
    fn gain(self) -> f32 {
        if self.muted { 0.0 } else { f32::from(self.volume.min(200)) / 100.0 }
    }
}

/// What the client must tell the UI after a voice state change.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Nothing,
    /// The voice status changed; rebuild the snapshot.
    Changed,
    /// The status changed and the user should know why.
    Notice(String),
}

#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    /// Send op 4.
    UpdateState(UpdateVoiceState),
    /// Start the driver connection, or move it.
    Connect(ConnectionInfo),
    /// Stop the driver, the microphone and playback.
    Disconnect,
    /// Our self mute and deafen changed.
    SelfState { mute: bool, deaf: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    /// `None` for DM and group calls.
    guild_id: Option<Id>,
    channel_id: Id,
}

impl Target {
    /// What the voice server calls the server: the guild, or the DM channel for calls.
    fn server(self) -> Id {
        self.guild_id.unwrap_or(self.channel_id)
    }
}

#[derive(Default)]
pub(crate) struct VoiceSession {
    /// The channel we want to be in.
    target: Option<Target>,
    status: VoiceStatus,
    self_mute: bool,
    self_deaf: bool,
    /// From our VOICE_STATE_UPDATE.
    voice_session_id: Option<String>,
    /// From VOICE_SERVER_UPDATE: endpoint and token.
    server: Option<(String, String)>,
    /// The connection the driver was last asked to make.
    connected: Option<ConnectionInfo>,
    actions: Vec<Action>,
}

impl VoiceSession {
    pub fn view(&self, store: &Store) -> Option<VoiceView> {
        let target = self.target?;
        let (guild_name, channel_name) = match target.guild_id {
            Some(guild_id) => {
                let guild = store.guilds.get(&guild_id);
                let channel_name = guild
                    .and_then(|guild| guild.channels.get(&target.channel_id))
                    .and_then(|channel| channel.name.clone())
                    .unwrap_or_default();
                (guild.map(|guild| guild.name.clone()).unwrap_or_default(), channel_name)
            }
            None => {
                let channel = store.private_channels.get(&target.channel_id);
                let kind = if channel.is_some_and(Store::is_group) { "Group call" } else { "Call" };
                (kind.to_owned(), channel.map(|channel| store.private_channel_name(channel)).unwrap_or_default())
            }
        };
        Some(VoiceView {
            guild_id: target.guild_id,
            channel_id: target.channel_id,
            guild_name,
            channel_name,
            status: self.status,
            self_mute: self.self_mute,
            self_deaf: self.self_deaf,
        })
    }

    /// Our mute and deafen, as chosen (deafening implies muting when sent to Discord).
    pub fn self_state(&self) -> (bool, bool) {
        (self.self_mute, self.self_deaf)
    }

    /// The actions queued since the last call.
    pub fn take_actions(&mut self) -> Vec<Action> {
        mem::take(&mut self.actions)
    }

    /// The channel we are connected to, once the voice handshake is done.
    pub fn connected_channel(&self) -> Option<Id> {
        self.target.filter(|_| self.status == VoiceStatus::Connected).map(|target| target.channel_id)
    }

    /// Joins a guild voice channel, or a DM or group call with `guild_id: None`.
    pub fn join(&mut self, guild_id: Option<Id>, channel_id: Id) -> Outcome {
        let new = Target { guild_id, channel_id };
        if let Some(target) = self.target
            && target.server() != new.server()
        {
            // A voice server serves one guild (or call): start over.
            self.actions.push(Action::Disconnect);
            self.connected = None;
            self.voice_session_id = None;
            self.server = None;
        }
        info!(?guild_id, %channel_id, "joining voice");
        self.target = Some(new);
        if self.status == VoiceStatus::Disconnected {
            self.status = VoiceStatus::Connecting;
        }
        self.send_state();
        Outcome::Changed
    }

    pub fn leave(&mut self) -> Outcome {
        let Some(target) = self.target else { return Outcome::Nothing };
        info!(guild_id = ?target.guild_id, "leaving voice");
        self.actions.push(Action::UpdateState(UpdateVoiceState {
            guild_id: target.guild_id,
            channel_id: None,
            self_mute: self.self_mute,
            self_deaf: self.self_deaf,
            self_video: false,
        }));
        self.reset();
        Outcome::Changed
    }

    pub fn set_self_mute(&mut self, mute: bool) -> Outcome {
        self.self_mute = mute;
        self.self_state_changed();
        Outcome::Changed
    }

    pub fn set_self_deaf(&mut self, deaf: bool) -> Outcome {
        self.self_deaf = deaf;
        self.self_state_changed();
        Outcome::Changed
    }

    fn self_state_changed(&mut self) {
        // Deafening also mutes, like in the official client.
        self.actions.push(Action::SelfState { mute: self.self_mute || self.self_deaf, deaf: self.self_deaf });
        self.send_state();
    }

    /// A fresh gateway session drops any voice connection of the previous one.
    pub fn on_new_session(&mut self) -> Outcome {
        if self.target.is_none() {
            return Outcome::Nothing;
        }
        self.reset();
        Outcome::Notice("Disconnected from voice after the connection to Discord was reset.".into())
    }

    /// `session_id` is our gateway session: voice states with another one belong to our other clients.
    pub fn on_voice_state(&mut self, state: &VoiceState, me: Id, session_id: &str) -> Outcome {
        if state.user_id != me || self.target.is_none() {
            return Outcome::Nothing;
        }
        if state.session_id != session_id {
            // Joining from another client takes the voice connection away from this one.
            if state.channel_id.is_some() {
                self.reset();
                return Outcome::Notice("Voice moved to another of your devices.".into());
            }
            return Outcome::Nothing;
        }
        if state.channel_id.is_none() && self.target.is_some_and(|target| target.guild_id != state.guild_id) {
            // We left the server we were in before; we are already on our way somewhere else.
            return Outcome::Nothing;
        }

        let Some(channel_id) = state.channel_id else {
            self.reset();
            return Outcome::Notice("Disconnected from voice.".into());
        };
        if let Some(target) = &mut self.target
            && target.channel_id != channel_id
        {
            info!(%channel_id, "moved to another voice channel");
            target.channel_id = channel_id;
        }
        self.voice_session_id = Some(state.session_id.clone());
        self.try_connect(me);
        Outcome::Changed
    }

    pub fn on_voice_server(&mut self, update: &VoiceServerUpdate, me: Id) -> Outcome {
        let Some(target) = self.target else { return Outcome::Nothing };
        let ours = match target.guild_id {
            Some(guild_id) => update.guild_id == Some(guild_id),
            None => update.guild_id.is_none() && update.channel_id.is_none_or(|id| id == target.channel_id),
        };
        if !ours {
            return Outcome::Nothing;
        }
        match &update.endpoint {
            Some(endpoint) => self.server = Some((endpoint.clone(), update.token.clone())),
            // The old server went away and a new one is being allocated; another update follows.
            None => {
                self.server = None;
                return Outcome::Nothing;
            }
        }
        self.try_connect(me);
        Outcome::Changed
    }

    pub fn on_voice_event(&mut self, event: &VoiceEvent) -> Outcome {
        if self.connected.is_none() {
            return Outcome::Nothing;
        }
        match event {
            VoiceEvent::Connected | VoiceEvent::Reconnected => {
                self.status = VoiceStatus::Connected;
                Outcome::Changed
            }
            VoiceEvent::Disconnected { reason: Some(reason) } => {
                self.leave();
                Outcome::Notice(format!("Lost the voice connection: {reason}"))
            }
            VoiceEvent::Disconnected { reason: None }
            | VoiceEvent::Speaking(_)
            | VoiceEvent::Transmitting { .. } => Outcome::Nothing,
        }
    }

    /// The driver finished (or failed) the handshake for `info`.
    pub fn on_connect_result(&mut self, info: &ConnectionInfo, result: Result<(), String>) -> Outcome {
        if self.connected.as_ref() != Some(info) {
            return Outcome::Nothing;
        }
        match result {
            Ok(()) => {
                self.status = VoiceStatus::Connected;
                Outcome::Changed
            }
            Err(e) => {
                warn!("voice connection failed: {e}");
                self.leave();
                Outcome::Notice(format!("Couldn't connect to voice: {e}"))
            }
        }
    }

    fn try_connect(&mut self, me: Id) {
        let (Some(target), Some((endpoint, token)), Some(session_id)) =
            (self.target, self.server.clone(), self.voice_session_id.clone())
        else {
            return;
        };
        let info = ConnectionInfo {
            server_id: target.server(),
            channel_id: target.channel_id,
            user_id: me,
            session_id,
            endpoint,
            token,
        };
        if self.connected.as_ref() == Some(&info) {
            return;
        }
        self.status = VoiceStatus::Connecting;
        self.connected = Some(info.clone());
        self.actions.push(Action::Connect(info));
    }

    fn send_state(&mut self) {
        let Some(target) = self.target else { return };
        self.actions.push(Action::UpdateState(UpdateVoiceState {
            guild_id: target.guild_id,
            channel_id: Some(target.channel_id),
            self_mute: self.self_mute || self.self_deaf,
            self_deaf: self.self_deaf,
            self_video: false,
        }));
    }

    /// Back to not being in voice. Mute and deafen persist, like in the official client.
    fn reset(&mut self) {
        self.actions.push(Action::Disconnect);
        self.target = None;
        self.status = VoiceStatus::Disconnected;
        self.voice_session_id = None;
        self.server = None;
        self.connected = None;
    }
}

/// Audio devices by id; `None` follows the system default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDevices {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// Owns the voice driver, the microphone and the output stream, and carries out [`Action`]s.
pub(crate) struct VoiceDriver {
    /// Where driver events and connection results are reported.
    events: mpsc::UnboundedSender<Internal>,
    transmit: Arc<TransmitControl>,
    devices: AudioDevices,
    voice: Option<Voice>,
    playback: Option<Playback>,
    capture: Option<Capture>,
    deafened: bool,
    user_audio: HashMap<Id, UserAudio>,
}

impl VoiceDriver {
    pub fn new(events: mpsc::UnboundedSender<Internal>, transmit: Arc<TransmitControl>) -> Self {
        Self {
            events,
            transmit,
            devices: AudioDevices::default(),
            voice: None,
            playback: None,
            capture: None,
            deafened: false,
            user_audio: HashMap::new(),
        }
    }

    pub fn perform(&mut self, action: Action, gateway: &Gateway) {
        match action {
            Action::UpdateState(update) => gateway.update_voice_state(&update),
            Action::Connect(info) => {
                if self.voice.is_none() {
                    let voice = self.create_voice();
                    self.voice = Some(voice);
                    self.start_capture(info.user_id);
                }
                let Some(voice) = &mut self.voice else { return };
                let connect = voice.connect(info.clone());
                let events = self.events.clone();
                tokio::spawn(async move {
                    let result = connect.await;
                    let _ = events.send(Internal::VoiceConnectResult { info, result });
                });
            }
            Action::Disconnect => {
                self.capture = None;
                if let Some(voice) = &mut self.voice {
                    voice.leave();
                }
                self.voice = None;
                self.playback = None;
            }
            Action::SelfState { mute, deaf } => {
                self.transmit.set_muted(mute);
                self.deafened = deaf;
                if let Some(voice) = &self.voice {
                    voice.set_deafened(deaf);
                }
            }
        }
    }

    /// Switches devices, reopening them if we are in voice.
    pub fn set_devices(&mut self, devices: AudioDevices, me: Option<Id>) {
        if devices == self.devices {
            return;
        }
        let input_changed = devices.input != self.devices.input;
        let output_changed = devices.output != self.devices.output;
        self.devices = devices;
        if self.voice.is_none() {
            return;
        }
        if output_changed {
            self.playback = None;
            let sink = self.open_playback();
            if let Some(voice) = &self.voice {
                voice.set_playback_sink(sink);
            }
        }
        if input_changed && let Some(me) = me {
            self.capture = None;
            self.start_capture(me);
        }
    }

    pub fn set_user_audio(&mut self, user_id: Id, audio: UserAudio) {
        self.user_audio.insert(user_id, audio);
        if let Some(voice) = &self.voice {
            voice.set_user_gain(user_id, audio.gain());
        }
    }

    fn open_playback(&mut self) -> Option<bardo_audio::PlaybackSink> {
        match Playback::start(self.devices.output.as_deref()) {
            Ok((playback, sink)) => {
                self.playback = Some(playback);
                Some(sink)
            }
            Err(e) => {
                warn!("could not open the output device: {e}");
                self.notice(format!("Couldn't open the output device: {e}"));
                None
            }
        }
    }

    /// Opens the microphone and feeds it, through the transmit gate, to the voice connection.
    fn start_capture(&mut self, me: Id) {
        let Some(voice) = &mut self.voice else { return };
        let mut feed = voice.start_microphone(me);
        let mut transmitter = Transmitter::new(self.transmit.clone());
        let started = Capture::start(self.devices.input.as_deref(), move |frame| {
            let open = transmitter.process(frame);
            feed.push(frame, open);
        });
        match started {
            Ok(capture) => self.capture = Some(capture),
            Err(e) => {
                warn!("could not open the microphone: {e}");
                self.notice(format!("Couldn't open the microphone: {e}"));
            }
        }
    }

    fn notice(&self, message: String) {
        let _ = self.events.send(Internal::Event(ClientEvent::Notice(message)));
    }

    fn create_voice(&mut self) -> Voice {
        let sink = self.open_playback();
        let events = self.events.clone();
        let on_event: EventCallback = Arc::new(move |event| {
            let _ = events.send(Internal::Voice(event));
        });
        let voice = Voice::new(sink, on_event);
        voice.set_deafened(self.deafened);
        for (&user_id, &audio) in &self.user_audio {
            voice.set_user_gain(user_id, audio.gain());
        }
        voice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Id = Id(1);
    const GUILD: Id = Id(10);
    const CHANNEL: Id = Id(20);
    const SESSION: &str = "gateway-session";

    fn state(channel_id: Option<Id>, session_id: &str) -> VoiceState {
        serde_json::from_value(serde_json::json!({
            "guild_id": GUILD,
            "channel_id": channel_id,
            "user_id": ME,
            "session_id": session_id,
        }))
        .unwrap()
    }

    fn server(endpoint: Option<&str>) -> VoiceServerUpdate {
        serde_json::from_value(serde_json::json!({
            "guild_id": GUILD,
            "token": "voice-token",
            "endpoint": endpoint,
        }))
        .unwrap()
    }

    fn joined() -> VoiceSession {
        let mut voice = VoiceSession::default();
        voice.join(Some(GUILD), CHANNEL);
        voice.take_actions();
        voice
    }

    fn connect_info(channel_id: Id) -> ConnectionInfo {
        ConnectionInfo {
            server_id: GUILD,
            channel_id,
            user_id: ME,
            session_id: SESSION.into(),
            endpoint: "voice.discord.media:443".into(),
            token: "voice-token".into(),
        }
    }

    #[test]
    fn join_sends_op_4_and_connects_once_both_updates_arrive() {
        let mut voice = VoiceSession::default();
        assert_eq!(voice.join(Some(GUILD), CHANNEL), Outcome::Changed);
        assert_eq!(
            voice.take_actions(),
            [Action::UpdateState(UpdateVoiceState {
                guild_id: Some(GUILD),
                channel_id: Some(CHANNEL),
                self_mute: false,
                self_deaf: false,
                self_video: false,
            })]
        );

        voice.on_voice_server(&server(Some("voice.discord.media:443")), ME);
        assert!(voice.take_actions().is_empty());
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        assert_eq!(voice.take_actions(), [Action::Connect(connect_info(CHANNEL))]);
        assert_eq!(voice.status, VoiceStatus::Connecting);

        // A repeated state update does not reconnect.
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        assert!(voice.take_actions().is_empty());

        voice.on_connect_result(&connect_info(CHANNEL), Ok(()));
        assert_eq!(voice.status, VoiceStatus::Connected);
    }

    #[test]
    fn being_moved_reconnects_to_the_new_channel() {
        let mut voice = joined();
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        voice.on_voice_server(&server(Some("voice.discord.media:443")), ME);
        voice.take_actions();

        voice.on_voice_state(&state(Some(Id(21)), SESSION), ME, SESSION);
        assert_eq!(voice.take_actions(), [Action::Connect(connect_info(Id(21)))]);
        assert_eq!(voice.view(&Store::default()).unwrap().channel_id, Id(21));
    }

    #[test]
    fn being_disconnected_resets() {
        let mut voice = joined();
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        let outcome = voice.on_voice_state(&state(None, SESSION), ME, SESSION);
        assert_eq!(outcome, Outcome::Notice("Disconnected from voice.".into()));
        assert_eq!(voice.take_actions(), [Action::Disconnect]);
        assert!(voice.view(&Store::default()).is_none());
    }

    #[test]
    fn other_clients_take_over_voice() {
        let mut voice = joined();
        // Our other client leaving voice is none of our business.
        assert_eq!(voice.on_voice_state(&state(None, "phone"), ME, SESSION), Outcome::Nothing);
        let outcome = voice.on_voice_state(&state(Some(CHANNEL), "phone"), ME, SESSION);
        assert!(matches!(outcome, Outcome::Notice(_)));
        assert_eq!(voice.take_actions(), [Action::Disconnect]);
    }

    #[test]
    fn server_reallocation_waits_for_the_new_endpoint() {
        let mut voice = joined();
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        assert_eq!(voice.on_voice_server(&server(None), ME), Outcome::Nothing);
        assert!(voice.take_actions().is_empty());
        voice.on_voice_server(&server(Some("voice.discord.media:443")), ME);
        assert_eq!(voice.take_actions(), [Action::Connect(connect_info(CHANNEL))]);
    }

    #[test]
    fn failed_connection_leaves_the_channel() {
        let mut voice = joined();
        voice.on_voice_state(&state(Some(CHANNEL), SESSION), ME, SESSION);
        voice.on_voice_server(&server(Some("voice.discord.media:443")), ME);
        voice.take_actions();

        let outcome = voice.on_connect_result(&connect_info(CHANNEL), Err("timed out".into()));
        assert_eq!(outcome, Outcome::Notice("Couldn't connect to voice: timed out".into()));
        let actions = voice.take_actions();
        assert!(matches!(&actions[0], Action::UpdateState(update) if update.channel_id.is_none()));
        assert_eq!(actions[1], Action::Disconnect);
    }

    #[test]
    fn dm_calls_use_the_channel_as_the_server() {
        const DM: Id = Id(50);
        let mut voice = joined();
        voice.join(None, DM);
        let actions = voice.take_actions();
        assert_eq!(actions[0], Action::Disconnect);
        assert!(matches!(&actions[1], Action::UpdateState(update) if update.guild_id.is_none() && update.channel_id == Some(DM)));

        // Leaving the guild we were in does not end the call we are joining.
        assert_eq!(voice.on_voice_state(&state(None, SESSION), ME, SESSION), Outcome::Nothing);

        let server = serde_json::from_value(serde_json::json!({
            "guild_id": null, "channel_id": DM, "token": "voice-token", "endpoint": "voice.discord.media:443",
        }))
        .unwrap();
        voice.on_voice_server(&server, ME);
        let call_state = serde_json::from_value(serde_json::json!({
            "guild_id": null, "channel_id": DM, "user_id": ME, "session_id": SESSION,
        }))
        .unwrap();
        voice.on_voice_state(&call_state, ME, SESSION);
        let info = ConnectionInfo { server_id: DM, ..connect_info(DM) };
        assert_eq!(voice.take_actions(), [Action::Connect(info.clone())]);
        voice.on_connect_result(&info, Ok(()));
        assert_eq!(voice.connected_channel(), Some(DM));

        voice.leave();
        assert!(matches!(&voice.take_actions()[0], Action::UpdateState(update) if update.guild_id.is_none() && update.channel_id.is_none()));
    }

    #[test]
    fn deafen_implies_mute() {
        let mut voice = joined();
        voice.set_self_deaf(true);
        let actions = voice.take_actions();
        assert_eq!(actions[0], Action::SelfState { mute: true, deaf: true });
        assert!(matches!(&actions[1], Action::UpdateState(update) if update.self_mute && update.self_deaf));

        voice.set_self_deaf(false);
        assert_eq!(voice.take_actions()[0], Action::SelfState { mute: false, deaf: false });
    }

    #[test]
    fn user_gain_follows_volume_and_mute() {
        assert_eq!(UserAudio { volume: 150, muted: false }.gain(), 1.5);
        assert_eq!(UserAudio { volume: 150, muted: true }.gain(), 0.0);
        assert_eq!(UserAudio { volume: 900, muted: false }.gain(), 2.0);
    }
}
