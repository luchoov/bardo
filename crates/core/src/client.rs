use std::{
    collections::HashSet,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use bardo_api::{Http, MemberChange};
use bardo_audio::TransmitControl;
use bardo_gateway::{
    CAPABILITIES, Config, ConnectionState, Event, Gateway, Id, Notification, RequestGuildMembers,
};
use bardo_voice::{ConnectionInfo, VoiceEvent};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    snapshot::Snapshot,
    store::Store,
    voice::{AudioDevices, Outcome, UserAudio, VoiceDriver, VoiceSession},
};

/// Op 8 accepts at most 100 user ids per request.
const MEMBER_REQUEST_LIMIT: usize = 100;
/// Time for the gateway to send our "leave voice" before the connection goes away.
const LEAVE_FLUSH: Duration = Duration::from_millis(300);
/// Upper bound on how long [`Client::shutdown`] waits.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum ClientEvent {
    State(ConnectionState),
    Snapshot(Arc<Snapshot>),
    /// The users currently heard speaking in our voice channel, sorted.
    Speaking(Vec<Id>),
    /// Discord rotated the token; it must be persisted in place of the old one.
    TokenRefreshed(String),
    /// Something the user should know about, such as a failed command.
    Notice(String),
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Server mute, server deafen, move or disconnect a guild member.
    ModifyMember { guild_id: Id, user_id: Id, change: MemberChange },
    /// Join a guild voice channel, or switch to it.
    JoinVoice { guild_id: Id, channel_id: Id },
    LeaveVoice,
    SetSelfMute(bool),
    SetSelfDeaf(bool),
    /// How a user sounds to us.
    SetUserAudio { user_id: Id, audio: UserAudio },
    SetAudioDevices(AudioDevices),
}

/// Results of background work, reported back to the client task.
#[derive(Debug)]
pub(crate) enum Internal {
    Event(ClientEvent),
    Voice(VoiceEvent),
    VoiceConnectResult { info: ConnectionInfo, result: Result<(), String> },
}

/// A running session. Its networking lives on a dedicated thread with its own tokio runtime;
/// dropping the client shuts it down.
pub struct Client {
    shutdown: Option<oneshot::Sender<()>>,
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Client {
    /// Connects with `token`. `transmit` holds the microphone settings and push-to-talk state;
    /// `on_event` is called from the client's thread.
    pub fn start(
        token: String,
        transmit: Arc<TransmitControl>,
        on_event: impl Fn(ClientEvent) + Send + 'static,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let spawned = thread::Builder::new().name("bardo-client".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    error!("failed to start tokio runtime: {e}");
                    return;
                }
            };
            runtime.block_on(run(token, transmit, on_event, commands_rx, shutdown_rx));
        });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(e) => {
                error!("failed to spawn client thread: {e}");
                None
            }
        };
        Self { shutdown: Some(shutdown_tx), commands: commands_tx, thread }
    }

    /// Disconnects and waits (briefly) until we have left voice. Dropping the client disconnects
    /// without waiting, which is fine while the process keeps running.
    pub fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(thread) = self.thread.take() else { return };
        let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Queues a command. Commands sent before the connection is up run once it is.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn run(
    token: String,
    transmit: Arc<TransmitControl>,
    on_event: impl Fn(ClientEvent),
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut shutdown: oneshot::Receiver<()>,
) {
    on_event(ClientEvent::State(ConnectionState::Connecting));
    let properties = tokio::select! {
        _ = &mut shutdown => return,
        properties = bardo_api::client_properties() => properties,
    };
    let http = match Http::new(&token, &properties) {
        Ok(http) => Arc::new(http),
        Err(e) => {
            error!("failed to build REST client: {e}");
            on_event(ClientEvent::State(ConnectionState::Failed(e.to_string())));
            return;
        }
    };
    let (gateway, mut notifications) = Gateway::spawn(Config {
        token,
        properties,
        capabilities: CAPABILITIES,
    });
    // Background tasks report back through this channel, so `on_event` is only called from here.
    let (internal_tx, mut internal) = mpsc::unbounded_channel();

    let mut session = Session::new(internal_tx.clone(), transmit);
    loop {
        let first = tokio::select! {
            _ = &mut shutdown => {
                // Leave voice explicitly, or we would linger in the channel until Discord notices.
                if session.leave_voice(&gateway, &on_event) {
                    tokio::time::sleep(LEAVE_FLUSH).await;
                }
                break;
            }
            Some(message) = internal.recv() => {
                session.on_internal(message, &gateway, &on_event);
                continue;
            }
            command = commands.recv() => match command {
                Some(command) => {
                    session.execute(command, &gateway, &http, &internal_tx, &on_event);
                    continue;
                }
                None => break,
            },
            notification = notifications.recv() => match notification {
                Some(notification) => notification,
                None => break,
            },
        };

        // Apply everything already queued before rebuilding the snapshot once.
        let mut dirty = false;
        for notification in std::iter::once(first).chain(drain(&mut notifications)) {
            match notification {
                Notification::State(state) => on_event(ClientEvent::State(state)),
                Notification::Event(Event::AuthTokenUpdate(update)) => {
                    info!("token rotated");
                    if let Err(e) = http.set_token(&update.token) {
                        warn!("failed to update the REST token: {e}");
                    }
                    on_event(ClientEvent::TokenRefreshed(update.token));
                }
                Notification::Event(event) => dirty |= session.apply(event, &gateway, &on_event),
            }
        }

        if dirty {
            session.request_unresolved_members(&gateway);
            session.publish(&on_event);
        }
    }
}

fn drain<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> impl Iterator<Item = T> + '_ {
    std::iter::from_fn(|| receiver.try_recv().ok())
}

struct Session {
    store: Store,
    /// Our gateway session id, from READY.
    session_id: String,
    voice: VoiceSession,
    driver: VoiceDriver,
    /// Users heard speaking, and whether we are sending, for the speaking indicator.
    remote_speaking: Vec<Id>,
    transmitting: bool,
    /// READY_SUPPLEMENTAL carries the members of everyone in voice; wait for it before asking for more.
    supplemental_received: bool,
    requested_members: HashSet<(Id, Id)>,
}

impl Session {
    fn new(internal: mpsc::UnboundedSender<Internal>, transmit: Arc<TransmitControl>) -> Self {
        Self {
            store: Store::default(),
            session_id: String::new(),
            voice: VoiceSession::default(),
            driver: VoiceDriver::new(internal, transmit),
            remote_speaking: Vec::new(),
            transmitting: false,
            supplemental_received: false,
            requested_members: HashSet::new(),
        }
    }

    fn publish(&self, on_event: &impl Fn(ClientEvent)) {
        let mut snapshot = Snapshot::build(&self.store);
        snapshot.voice = self.voice.view(&self.store);
        on_event(ClientEvent::Snapshot(Arc::new(snapshot)));
    }

    /// Leaves voice if we are in a channel. Returns whether we were.
    fn leave_voice(&mut self, gateway: &Gateway, on_event: &impl Fn(ClientEvent)) -> bool {
        let outcome = self.voice.leave();
        let was_in_voice = outcome != Outcome::Nothing;
        self.settle_voice(outcome, gateway, on_event);
        was_in_voice
    }

    fn publish_speaking(&self, on_event: &impl Fn(ClientEvent)) {
        let mut speaking = self.remote_speaking.clone();
        if self.transmitting
            && let Some(me) = &self.store.me
        {
            speaking.push(me.id);
        }
        on_event(ClientEvent::Speaking(speaking));
    }

    /// Carries out what the voice state machine decided, then reports the outcome to the UI.
    /// Returns whether the snapshot must be rebuilt.
    fn settle_voice(&mut self, outcome: Outcome, gateway: &Gateway, on_event: &impl Fn(ClientEvent)) -> bool {
        for action in self.voice.take_actions() {
            self.driver.perform(action, gateway);
        }
        match outcome {
            Outcome::Nothing => false,
            Outcome::Changed => true,
            Outcome::Notice(message) => {
                on_event(ClientEvent::Notice(message));
                true
            }
        }
    }

    fn apply(&mut self, event: Event, gateway: &Gateway, on_event: &impl Fn(ClientEvent)) -> bool {
        let me = self.store.me.as_ref().map(|user| user.id);
        let outcome = match (&event, me) {
            (Event::Ready(ready), _) => {
                self.session_id = ready.session_id.clone();
                self.supplemental_received = false;
                self.requested_members.clear();
                self.voice.on_new_session()
            }
            (Event::ReadySupplemental(_), _) => {
                self.supplemental_received = true;
                Outcome::Nothing
            }
            (Event::VoiceStateUpdate(state), Some(me)) => self.voice.on_voice_state(state, me, &self.session_id),
            (Event::VoiceServerUpdate(update), Some(me)) => self.voice.on_voice_server(update, me),
            _ => Outcome::Nothing,
        };
        let voice_changed = self.settle_voice(outcome, gateway, on_event);
        self.store.apply(event) | voice_changed
    }

    fn on_internal(&mut self, message: Internal, gateway: &Gateway, on_event: &impl Fn(ClientEvent)) {
        let outcome = match message {
            Internal::Event(event) => {
                on_event(event);
                return;
            }
            Internal::Voice(VoiceEvent::Speaking(users)) => {
                self.remote_speaking = users;
                self.publish_speaking(on_event);
                return;
            }
            Internal::Voice(VoiceEvent::Transmitting { transmitting, .. }) => {
                self.transmitting = transmitting;
                self.publish_speaking(on_event);
                return;
            }
            Internal::Voice(event) => self.voice.on_voice_event(&event),
            Internal::VoiceConnectResult { info, result } => self.voice.on_connect_result(&info, result),
        };
        if self.settle_voice(outcome, gateway, on_event) {
            self.publish(on_event);
        }
    }

    fn execute(
        &mut self,
        command: Command,
        gateway: &Gateway,
        http: &Arc<Http>,
        internal: &mpsc::UnboundedSender<Internal>,
        on_event: &impl Fn(ClientEvent),
    ) {
        let outcome = match command {
            Command::ModifyMember { guild_id, user_id, change } => {
                let action = self.describe_member_change(guild_id, user_id, change);
                info!(%guild_id, %user_id, ?change, "modifying member");
                let (http, internal) = (http.clone(), internal.clone());
                tokio::spawn(async move {
                    if let Err(e) = http.modify_member(guild_id, user_id, change).await {
                        warn!(%guild_id, %user_id, ?change, "member change failed: {e}");
                        let notice = ClientEvent::Notice(format!("Couldn't {action}: {e}"));
                        let _ = internal.send(Internal::Event(notice));
                    }
                });
                return;
            }
            Command::JoinVoice { guild_id, channel_id } => self.voice.join(guild_id, channel_id),
            Command::LeaveVoice => self.voice.leave(),
            Command::SetSelfMute(mute) => self.voice.set_self_mute(mute),
            Command::SetSelfDeaf(deaf) => self.voice.set_self_deaf(deaf),
            Command::SetUserAudio { user_id, audio } => {
                self.driver.set_user_audio(user_id, audio);
                return;
            }
            Command::SetAudioDevices(devices) => {
                self.driver.set_devices(devices, self.store.me.as_ref().map(|me| me.id));
                return;
            }
        };
        if self.settle_voice(outcome, gateway, on_event) {
            self.publish(on_event);
        }
    }

    /// Describes a member change for error messages, e.g. "move Zed to General".
    fn describe_member_change(&self, guild_id: Id, user_id: Id, change: MemberChange) -> String {
        let guild = self.store.guilds.get(&guild_id);
        let name = guild
            .and_then(|guild| self.store.display_name(guild, user_id))
            .unwrap_or_else(|| user_id.to_string());
        match change {
            MemberChange::ServerMute(true) => format!("server mute {name}"),
            MemberChange::ServerMute(false) => format!("remove the server mute from {name}"),
            MemberChange::ServerDeafen(true) => format!("server deafen {name}"),
            MemberChange::ServerDeafen(false) => format!("remove the server deafen from {name}"),
            MemberChange::Move(None) => format!("disconnect {name}"),
            MemberChange::Move(Some(channel_id)) => {
                let channel = guild
                    .and_then(|guild| guild.channels.get(&channel_id))
                    .and_then(|channel| channel.name.clone())
                    .unwrap_or_else(|| channel_id.to_string());
                format!("move {name} to {channel}")
            }
        }
    }

    fn request_unresolved_members(&mut self, gateway: &Gateway) {
        if !self.supplemental_received {
            return;
        }
        for (guild_id, users) in self.store.unresolved_voice_users() {
            let users: Vec<Id> = users
                .into_iter()
                .filter(|&user_id| self.requested_members.insert((guild_id, user_id)))
                .collect();
            for user_ids in users.chunks(MEMBER_REQUEST_LIMIT) {
                debug!(%guild_id, count = user_ids.len(), "requesting guild members");
                gateway.request_guild_members(&RequestGuildMembers {
                    guild_id,
                    user_ids: user_ids.to_vec(),
                    presences: false,
                });
            }
        }
    }
}
