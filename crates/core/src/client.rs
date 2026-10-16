use std::{
    collections::HashSet,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use bardo_api::{Http, MemberChange, Release};
use bardo_audio::{Sound, TransmitControl};
use bardo_gateway::{
    CAPABILITIES, Config, ConnectionState, Event, Gateway, Id, Notification, RequestGuildMembers,
};
use bardo_voice::{ConnectionInfo, VoiceEvent};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    cues,
    snapshot::Snapshot,
    store::Store,
    voice::{Action, AudioDevices, Direction, Outcome, UserAudio, VoiceDriver, VoiceSession},
};

/// Op 8 accepts at most 100 user ids per request.
const MEMBER_REQUEST_LIMIT: usize = 100;
/// Time for the gateway to send our "leave voice" before the connection goes away.
const LEAVE_FLUSH: Duration = Duration::from_millis(300);
/// Upper bound on how long [`Client::shutdown`] waits.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// When the gateway has to start a fresh session, voice is joined again if the connection was down
/// for less than this; after longer, the user may well have moved on.
const REJOIN_WINDOW: Duration = Duration::from_secs(180);

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
    /// A cue for something that happened in voice: we joined, someone left, we were moved…
    Sound(Sound),
    /// A newer bardo is out. Checked once per sign-in.
    UpdateAvailable(Release),
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Server mute, server deafen, move or disconnect a guild member.
    ModifyMember { guild_id: Id, user_id: Id, change: MemberChange },
    /// Join a guild voice channel, or a DM or group call with `guild_id: None`, or switch to it.
    JoinVoice { guild_id: Option<Id>, channel_id: Id },
    /// Join a DM or group call and ring the others, unless someone is already in it.
    StartCall { channel_id: Id },
    /// Stop a call from ringing us.
    DeclineCall { channel_id: Id },
    LeaveVoice,
    SetSelfMute(bool),
    SetSelfDeaf(bool),
    /// How a user sounds to us.
    SetUserAudio { user_id: Id, audio: UserAudio },
    SetAudioDevices(AudioDevices),
    /// The guilds being shown in full. Missing member data (names) is only fetched for these and
    /// for the guild we are in voice in, so big public servers cost nothing while hidden.
    SetWatchedGuilds(HashSet<Id>),
}

/// Results of background work, reported back to the client task.
#[derive(Debug)]
pub(crate) enum Internal {
    Event(ClientEvent),
    Voice(VoiceEvent),
    VoiceConnectResult { info: ConnectionInfo, result: Result<(), String> },
    /// An audio stream was lost, or a device that failed to open is due another try.
    ReopenDevice { direction: Direction, generation: u64 },
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

    let mut session = Session::new(internal_tx.clone(), transmit, http.clone());
    tokio::spawn(check_for_update(internal_tx.clone()));
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
                Notification::State(state) => {
                    session.on_connection_state(&state);
                    on_event(ClientEvent::State(state));
                }
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

/// Asks GitHub whether a newer bardo is out. Every crate shares the workspace version.
async fn check_for_update(internal: mpsc::UnboundedSender<Internal>) {
    match bardo_api::newer_release(env!("CARGO_PKG_VERSION")).await {
        Ok(Some(release)) => {
            info!(version = release.version, "a newer release is out");
            let _ = internal.send(Internal::Event(ClientEvent::UpdateAvailable(release)));
        }
        Ok(None) => {}
        Err(e) => debug!("could not check for a newer release: {e}"),
    }
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
    watched_guilds: HashSet<Id>,
    http: Arc<Http>,
    internal: mpsc::UnboundedSender<Internal>,
    /// A call we started: the others are rung once we are connected.
    ring_when_connected: Option<Id>,
    /// The last snapshot sent, to tell who came and went.
    published: Arc<Snapshot>,
    /// Since when the gateway has been reconnecting, until the session is back.
    offline_since: Option<Instant>,
}

impl Session {
    fn new(internal: mpsc::UnboundedSender<Internal>, transmit: Arc<TransmitControl>, http: Arc<Http>) -> Self {
        Self {
            http,
            internal: internal.clone(),
            ring_when_connected: None,
            store: Store::default(),
            session_id: String::new(),
            voice: VoiceSession::default(),
            driver: VoiceDriver::new(internal, transmit),
            remote_speaking: Vec::new(),
            transmitting: false,
            supplemental_received: false,
            requested_members: HashSet::new(),
            watched_guilds: HashSet::new(),
            published: Arc::default(),
            offline_since: None,
        }
    }

    fn on_connection_state(&mut self, state: &ConnectionState) {
        if matches!(state, ConnectionState::Reconnecting { .. }) {
            self.offline_since.get_or_insert_with(Instant::now);
        }
    }

    fn publish(&mut self, on_event: &impl Fn(ClientEvent)) {
        let mut snapshot = Snapshot::build(&self.store);
        snapshot.voice = self.voice.view(&self.store);
        (snapshot.self_mute, snapshot.self_deaf) = self.voice.self_state();
        let snapshot = Arc::new(snapshot);
        for sound in cues::between(&self.published, &snapshot) {
            on_event(ClientEvent::Sound(sound));
        }
        self.published = snapshot.clone();
        on_event(ClientEvent::Snapshot(snapshot));
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
            // The old connection's speakers would stay lit until someone speaks on the next one.
            if action == Action::Disconnect && !self.remote_speaking.is_empty() {
                self.remote_speaking.clear();
                self.publish_speaking(on_event);
            }
            self.driver.perform(action, gateway);
        }
        for sound in self.voice.take_cues() {
            on_event(ClientEvent::Sound(sound));
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
                let rejoin = self.offline_since.take().is_some_and(|since| since.elapsed() < REJOIN_WINDOW);
                self.voice.on_new_session(rejoin)
            }
            (Event::Resumed, _) => {
                self.offline_since = None;
                Outcome::Nothing
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
            Internal::ReopenDevice { direction, generation } => {
                self.driver.reopen(direction, generation);
                return;
            }
        };
        if self.settle_voice(outcome, gateway, on_event) {
            self.publish(on_event);
        }
        self.ring_if_connected();
    }

    fn ring_if_connected(&mut self) {
        let Some(channel_id) = self.ring_when_connected else { return };
        if self.voice.connected_channel() != Some(channel_id) {
            return;
        }
        self.ring_when_connected = None;
        info!(%channel_id, "ringing");
        self.spawn_request("ring", move |http| async move { http.ring(channel_id, None).await });
    }

    /// Runs a REST request in the background and reports a failure as a notice.
    fn spawn_request<F, Fut>(&self, what: &'static str, request: F)
    where
        F: FnOnce(Arc<Http>) -> Fut,
        Fut: Future<Output = Result<(), bardo_api::Error>> + Send + 'static,
    {
        let internal = self.internal.clone();
        let request = request(self.http.clone());
        tokio::spawn(async move {
            if let Err(e) = request.await {
                warn!("{what} failed: {e}");
                let _ = internal.send(Internal::Event(ClientEvent::Notice(format!("Couldn't {what}: {e}"))));
            }
        });
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
            Command::JoinVoice { guild_id, channel_id } => {
                self.ring_when_connected = None;
                self.voice.join(guild_id, channel_id)
            }
            Command::StartCall { channel_id } => {
                let me = self.store.me.as_ref().map(|me| me.id);
                let someone_there = self.store.calls.get(&channel_id).is_some_and(|call| {
                    call.voice_states.keys().any(|&user_id| Some(user_id) != me)
                });
                self.ring_when_connected = (!someone_there).then_some(channel_id);
                self.voice.join(None, channel_id)
            }
            Command::DeclineCall { channel_id } => {
                let Some(me) = self.store.me.as_ref().map(|me| me.id) else { return };
                self.spawn_request("decline the call", move |http| async move {
                    http.stop_ringing(channel_id, &[me]).await
                });
                return;
            }
            Command::LeaveVoice => {
                self.ring_when_connected = None;
                self.voice.leave()
            }
            Command::SetSelfMute(mute) => self.voice.set_self_mute(mute),
            Command::SetSelfDeaf(deaf) => self.voice.set_self_deaf(deaf),
            Command::SetUserAudio { user_id, audio } => {
                self.driver.set_user_audio(user_id, audio);
                return;
            }
            Command::SetAudioDevices(devices) => {
                self.driver.set_devices(devices);
                return;
            }
            Command::SetWatchedGuilds(guilds) => {
                self.watched_guilds = guilds;
                self.request_unresolved_members(gateway);
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
        let in_voice = self.voice.view(&self.store).and_then(|voice| voice.guild_id);
        let wanted = |guild_id| self.watched_guilds.contains(&guild_id) || in_voice == Some(guild_id);
        for (guild_id, users) in self.store.unresolved_voice_users(wanted) {
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
