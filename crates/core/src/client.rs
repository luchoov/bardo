use std::{collections::HashSet, sync::Arc, thread};

use bardo_gateway::{
    CAPABILITIES, Config, ConnectionState, Event, Gateway, Id, Notification, RequestGuildMembers,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error};

use crate::{snapshot::Snapshot, store::Store};

/// Op 8 accepts at most 100 user ids per request.
const MEMBER_REQUEST_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub enum ClientEvent {
    State(ConnectionState),
    Snapshot(Arc<Snapshot>),
    /// Discord rotated the token; it must be persisted in place of the old one.
    TokenRefreshed(String),
}

/// A running session. Its networking lives on a dedicated thread with its own tokio runtime;
/// dropping the client shuts it down.
pub struct Client {
    shutdown: Option<oneshot::Sender<()>>,
}

impl Client {
    /// Connects with `token`. `on_event` is called from the client's thread.
    pub fn start(token: String, on_event: impl Fn(ClientEvent) + Send + 'static) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
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
            runtime.block_on(run(token, on_event, shutdown_rx));
        });
        if let Err(e) = spawned {
            error!("failed to spawn client thread: {e}");
        }
        Self { shutdown: Some(shutdown_tx) }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn run(token: String, on_event: impl Fn(ClientEvent), mut shutdown: oneshot::Receiver<()>) {
    on_event(ClientEvent::State(ConnectionState::Connecting));
    let properties = tokio::select! {
        _ = &mut shutdown => return,
        properties = bardo_api::client_properties() => properties,
    };
    let (gateway, mut notifications) = Gateway::spawn(Config {
        token,
        properties,
        capabilities: CAPABILITIES,
    });

    let mut session = Session::default();
    loop {
        let first = tokio::select! {
            _ = &mut shutdown => break,
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
