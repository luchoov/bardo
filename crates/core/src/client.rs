use std::{collections::HashSet, sync::Arc, thread};

use bardo_api::{Http, MemberChange};
use bardo_gateway::{
    CAPABILITIES, Config, ConnectionState, Event, Gateway, Id, Notification, RequestGuildMembers,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{snapshot::Snapshot, store::Store};

/// Op 8 accepts at most 100 user ids per request.
const MEMBER_REQUEST_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub enum ClientEvent {
    State(ConnectionState),
    Snapshot(Arc<Snapshot>),
    /// Discord rotated the token; it must be persisted in place of the old one.
    TokenRefreshed(String),
    /// A command failed; the message is meant for the user.
    CommandFailed(String),
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Server mute, server deafen, move or disconnect a guild member.
    ModifyMember { guild_id: Id, user_id: Id, change: MemberChange },
}

/// A running session. Its networking lives on a dedicated thread with its own tokio runtime;
/// dropping the client shuts it down.
pub struct Client {
    shutdown: Option<oneshot::Sender<()>>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Client {
    /// Connects with `token`. `on_event` is called from the client's thread.
    pub fn start(token: String, on_event: impl Fn(ClientEvent) + Send + 'static) -> Self {
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
            runtime.block_on(run(token, on_event, commands_rx, shutdown_rx));
        });
        if let Err(e) = spawned {
            error!("failed to spawn client thread: {e}");
        }
        Self { shutdown: Some(shutdown_tx), commands: commands_tx }
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
    let (results_tx, mut results) = mpsc::unbounded_channel();

    let mut session = Session::default();
    loop {
        let first = tokio::select! {
            _ = &mut shutdown => break,
            Some(event) = results.recv() => {
                on_event(event);
                continue;
            }
            command = commands.recv() => match command {
                Some(command) => {
                    session.execute(command, &http, &results_tx);
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
                Notification::Event(event) => dirty |= session.apply(event),
            }
        }

        if dirty {
            session.request_unresolved_members(&gateway);
            on_event(ClientEvent::Snapshot(Arc::new(Snapshot::build(&session.store))));
        }
    }
}

fn drain<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> impl Iterator<Item = T> + '_ {
    std::iter::from_fn(|| receiver.try_recv().ok())
}

#[derive(Default)]
struct Session {
    store: Store,
    /// READY_SUPPLEMENTAL carries the members of everyone in voice; wait for it before asking for more.
    supplemental_received: bool,
    requested_members: HashSet<(Id, Id)>,
}

impl Session {
    fn apply(&mut self, event: Event) -> bool {
        match &event {
            Event::Ready(_) => {
                self.supplemental_received = false;
                self.requested_members.clear();
            }
            Event::ReadySupplemental(_) => self.supplemental_received = true,
            _ => {}
        }
        self.store.apply(event)
    }

    fn execute(&mut self, command: Command, http: &Arc<Http>, results: &mpsc::UnboundedSender<ClientEvent>) {
        match command {
            Command::ModifyMember { guild_id, user_id, change } => {
                let action = self.describe_member_change(guild_id, user_id, change);
                info!(%guild_id, %user_id, ?change, "modifying member");
                let (http, results) = (http.clone(), results.clone());
                tokio::spawn(async move {
                    if let Err(e) = http.modify_member(guild_id, user_id, change).await {
                        warn!(%guild_id, %user_id, ?change, "member change failed: {e}");
                        let _ = results.send(ClientEvent::CommandFailed(format!("Couldn't {action}: {e}")));
                    }
                });
            }
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
