//! Our own voice connection. Joining takes three steps: op 4 asks the gateway to put us in a
//! channel, the gateway answers with our VOICE_STATE_UPDATE (voice session id) and a
//! VOICE_SERVER_UPDATE (endpoint and token), and only then can the voice driver connect.
//!
//! [`VoiceSession`] is the state machine; it emits [`Action`]s that [`VoiceDriver`] carries out.

use std::{collections::HashMap, mem, sync::Arc};

use bardo_audio::Playback;
use bardo_gateway::{Gateway, Id, UpdateVoiceState, VoiceServerUpdate, VoiceState};
use bardo_voice::{ConnectionInfo, EventCallback, Voice, VoiceEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    client::Internal,
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
    /// Stop the driver and close playback.
    Disconnect,
    SetDeafened(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    guild_id: Id,
    channel_id: Id,
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
        let guild = store.guilds.get(&target.guild_id);
        let channel_name = guild
            .and_then(|guild| guild.channels.get(&target.channel_id))
            .and_then(|channel| channel.name.clone())
            .unwrap_or_default();
        Some(VoiceView {
            guild_id: target.guild_id,
            channel_id: target.channel_id,
            guild_name: guild.map(|guild| guild.name.clone()).unwrap_or_default(),
            channel_name,
            status: self.status,
            self_mute: self.self_mute,
            self_deaf: self.self_deaf,
        })
    }

    /// The actions queued since the last call.
    pub fn take_actions(&mut self) -> Vec<Action> {
        mem::take(&mut self.actions)
    }

    pub fn join(&mut self, guild_id: Id, channel_id: Id) -> Outcome {
        if let Some(target) = self.target
            && target.guild_id != guild_id
        {
            // A voice server serves one guild: start over.
            self.actions.push(Action::Disconnect);
            self.connected = None;
            self.voice_session_id = None;
            self.server = None;
        }
        info!(%guild_id, %channel_id, "joining voice");
        self.target = Some(Target { guild_id, channel_id });
        if self.status == VoiceStatus::Disconnected {
            self.status = VoiceStatus::Connecting;
        }
        self.send_state();
        Outcome::Changed
    }

    pub fn leave(&mut self) -> Outcome {
        let Some(target) = self.target else { return Outcome::Nothing };
        info!(guild_id = %target.guild_id, "leaving voice");
        self.actions.push(Action::UpdateState(UpdateVoiceState {
            guild_id: Some(target.guild_id),
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
        self.send_state();
        Outcome::Changed
    }

    pub fn set_self_deaf(&mut self, deaf: bool) -> Outcome {
        self.self_deaf = deaf;
        self.actions.push(Action::SetDeafened(deaf));
        self.send_state();
        Outcome::Changed
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
        if update.guild_id != Some(target.guild_id) {
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
            VoiceEvent::Disconnected { reason: None } | VoiceEvent::Speaking(_) => Outcome::Nothing,
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
            server_id: target.guild_id,
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
            guild_id: Some(target.guild_id),
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

/// Owns the voice driver and the output stream, and carries out [`Action`]s.
pub(crate) struct VoiceDriver {
    /// Where driver events and connection results are reported.
    events: mpsc::UnboundedSender<Internal>,
    voice: Option<Voice>,
    playback: Option<Playback>,
    deafened: bool,
    user_audio: HashMap<Id, UserAudio>,
}

impl VoiceDriver {
    pub fn new(events: mpsc::UnboundedSender<Internal>) -> Self {
        Self { events, voice: None, playback: None, deafened: false, user_audio: HashMap::new() }
    }

    pub fn perform(&mut self, action: Action, gateway: &Gateway) {
        match action {
            Action::UpdateState(update) => gateway.update_voice_state(&update),
            Action::Connect(info) => {
                let voice = match &mut self.voice {
                    Some(voice) => voice,
                    None => {
                        let voice = self.create_voice();
                        self.voice.insert(voice)
                    }
                };
                let connect = voice.connect(info.clone());
                let events = self.events.clone();
                tokio::spawn(async move {
                    let result = connect.await;
                    let _ = events.send(Internal::VoiceConnectResult { info, result });
                });
            }
            Action::Disconnect => {
                if let Some(voice) = &mut self.voice {
                    voice.leave();
                }
                self.voice = None;
                self.playback = None;
            }
            Action::SetDeafened(deafened) => {
                self.deafened = deafened;
                if let Some(voice) = &self.voice {
                    voice.set_deafened(deafened);
                }
            }
        }
    }

    pub fn set_user_audio(&mut self, user_id: Id, audio: UserAudio) {
        self.user_audio.insert(user_id, audio);
        if let Some(voice) = &self.voice {
            voice.set_user_gain(user_id, audio.gain());
        }
    }

    fn create_voice(&mut self) -> Voice {
        let sink = match Playback::start(None) {
            Ok((playback, sink)) => {
                self.playback = Some(playback);
                Some(sink)
            }
            Err(e) => {
                warn!("could not open the output device: {e}");
                None
            }
        };
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
        voice.join(GUILD, CHANNEL);
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
        assert_eq!(voice.join(GUILD, CHANNEL), Outcome::Changed);
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
    fn deafen_implies_mute() {
        let mut voice = joined();
        voice.set_self_deaf(true);
        let actions = voice.take_actions();
        assert_eq!(actions[0], Action::SetDeafened(true));
        assert!(matches!(&actions[1], Action::UpdateState(update) if update.self_mute && update.self_deaf));
    }

    #[test]
    fn user_gain_follows_volume_and_mute() {
        assert_eq!(UserAudio { volume: 150, muted: false }.gain(), 1.5);
        assert_eq!(UserAudio { volume: 150, muted: true }.gain(), 0.0);
        assert_eq!(UserAudio { volume: 900, muted: false }.gain(), 2.0);
    }
}
