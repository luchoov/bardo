use std::{collections::VecDeque, path::PathBuf, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        self, Message,
        client::IntoClientRequest,
        http::HeaderValue,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use tracing::{debug, info, trace, warn};

use crate::{
    event::Event,
    inflate::Inflater,
    model::{RequestGuildMembers, UpdateVoiceState},
    properties::ClientProperties,
};

const GATEWAY_URL: &str = "wss://gateway.discord.gg";
const GATEWAY_QUERY: &str = "/?encoding=json&v=9&compress=zlib-stream";

/// Gateway capabilities, matching what current user-account clients send:
/// LAZY_USER_NOTES, VERSIONED_READ_STATES, VERSIONED_USER_GUILD_SETTINGS, DEDUPE_USER_OBJECTS,
/// PRIORITIZED_READY_PAYLOAD, MULTIPLE_GUILD_EXPERIMENT_POPULATIONS, NON_CHANNEL_READ_STATES,
/// AUTH_TOKEN_REFRESH, USER_SETTINGS_PROTO, CLIENT_STATE_V2, AUTO_CALL_CONNECT, PASSIVE_GUILD_UPDATE_V2.
pub const CAPABILITIES: u64 = 1 << 0
    | 1 << 2
    | 1 << 3
    | 1 << 4
    | 1 << 5
    | 1 << 6
    | 1 << 7
    | 1 << 8
    | 1 << 9
    | 1 << 10
    | 1 << 12
    | 1 << 14;

/// Discord allows 120 gateway sends per 60 s; keep headroom for heartbeats.
const SEND_BUDGET: usize = 110;
const SEND_WINDOW: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Close code we use to drop a connection while keeping the session resumable.
const CLOSE_RESUMABLE: u16 = 4000;

pub struct Config {
    pub token: String,
    pub properties: ClientProperties,
    pub capabilities: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    /// The token was rejected (close code 4004). The gateway task has stopped.
    AuthenticationFailed,
    /// An unrecoverable close code. The gateway task has stopped.
    Failed(String),
}

#[derive(Debug)]
pub enum Notification {
    State(ConnectionState),
    Event(Event),
}

/// Handle to the gateway task. Dropping it closes the connection.
pub struct Gateway {
    commands: mpsc::UnboundedSender<Value>,
}

impl Gateway {
    /// Spawns the connection task on the current tokio runtime.
    pub fn spawn(config: Config) -> (Self, mpsc::UnboundedReceiver<Notification>) {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let runner = Runner {
            config,
            commands: commands_rx,
            notify: notify_tx,
            session: None,
            seq: None,
            attempt: 0,
            outbox: VecDeque::new(),
            sent: VecDeque::new(),
            dump_dir: std::env::var_os("BARDO_DUMP_DIR").map(PathBuf::from),
        };
        tokio::spawn(runner.run());
        (Self { commands: commands_tx }, notify_rx)
    }

    pub fn update_voice_state(&self, update: &UpdateVoiceState) {
        self.send(4, update);
    }

    pub fn request_guild_members(&self, request: &RequestGuildMembers) {
        self.send(8, request);
    }

    fn send(&self, op: u8, data: &impl Serialize) {
        let _ = self.commands.send(json!({ "op": op, "d": data }));
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("websocket: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("decompression: {0}")]
    Inflate(#[from] std::io::Error),
    #[error("invalid payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed (code {0:?})")]
    Closed(Option<u16>),
    #[error("heartbeat was not acknowledged")]
    Zombie,
    #[error("expected Hello, got op {0}")]
    NoHello(u8),
}

enum Exit {
    Shutdown,
    Reconnect,
    Fatal(ConnectionState),
}

struct Session {
    id: String,
    resume_url: String,
}

#[derive(Deserialize)]
struct Payload<'a> {
    op: u8,
    #[serde(borrow, default)]
    d: Option<&'a RawValue>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<&'a str>,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Runner {
    config: Config,
    commands: mpsc::UnboundedReceiver<Value>,
    notify: mpsc::UnboundedSender<Notification>,
    session: Option<Session>,
    seq: Option<u64>,
    attempt: u32,
    /// Commands waiting for a free slot in the send budget. Survives reconnects.
    outbox: VecDeque<Value>,
    /// Send timestamps inside the current rate limit window.
    sent: VecDeque<Instant>,
    dump_dir: Option<PathBuf>,
}

impl Runner {
    async fn run(mut self) {
        loop {
            self.set_state(match self.attempt {
                0 => ConnectionState::Connecting,
                attempt => ConnectionState::Reconnecting { attempt },
            });

            match self.connection().await {
                Exit::Shutdown => return,
                Exit::Fatal(state) => {
                    self.set_state(state);
                    return;
                }
                Exit::Reconnect => {}
            }
            if self.notify.is_closed() || self.commands.is_closed() {
                return;
            }

            self.attempt += 1;
            let delay = backoff(self.attempt);
            debug!(attempt = self.attempt, ?delay, "reconnecting");
            time::sleep(delay).await;
        }
    }

    async fn connection(&mut self) -> Exit {
        let base = self.session.as_ref().map_or(GATEWAY_URL, |s| s.resume_url.as_str());
        let url = format!("{}{GATEWAY_QUERY}", base.trim_end_matches('/'));
        let mut request = match url.into_client_request() {
            Ok(request) => request,
            Err(e) => return Exit::Fatal(ConnectionState::Failed(format!("bad gateway url: {e}"))),
        };
        let headers = request.headers_mut();
        if let Ok(user_agent) = HeaderValue::from_str(self.config.properties.user_agent()) {
            headers.insert("User-Agent", user_agent);
        }
        headers.insert("Origin", HeaderValue::from_static("https://discord.com"));

        let mut socket = match connect_async(request).await {
            Ok((socket, _)) => socket,
            Err(e) => {
                warn!("gateway connect failed: {e}");
                return Exit::Reconnect;
            }
        };

        let exit = match self.drive(&mut socket).await {
            Ok(exit) => exit,
            Err(Error::Closed(Some(code))) => self.on_close_code(code),
            Err(e) => {
                warn!("gateway connection lost: {e}");
                Exit::Reconnect
            }
        };

        let code = match exit {
            Exit::Shutdown => CloseCode::Normal,
            _ => CloseCode::from(CLOSE_RESUMABLE),
        };
        let frame = CloseFrame { code, reason: "".into() };
        let _ = time::timeout(CLOSE_TIMEOUT, socket.close(Some(frame))).await;
        exit
    }

    fn on_close_code(&mut self, code: u16) -> Exit {
        warn!(code, "gateway closed the connection");
        match code {
            4004 => Exit::Fatal(ConnectionState::AuthenticationFailed),
            4010..=4014 => Exit::Fatal(ConnectionState::Failed(format!("gateway close code {code}"))),
            // Invalid sequence or session timed out: start a fresh session.
            4007 | 4009 => {
                self.session = None;
                self.seq = None;
                Exit::Reconnect
            }
            _ => Exit::Reconnect,
        }
    }

    async fn drive(&mut self, socket: &mut Socket) -> Result<Exit, Error> {
        let mut inflater = Inflater::new();

        let hello = loop {
            let Some(message) = socket.next().await else {
                return Err(Error::Closed(None));
            };
            let message = message?;
            let Some(bytes) = frame_bytes(&message, &mut inflater)? else {
                continue;
            };
            let payload: Payload = serde_json::from_slice(bytes)?;
            if payload.op != 10 {
                return Err(Error::NoHello(payload.op));
            }
            let data = payload.d.map_or("null", |d| d.get());
            break serde_json::from_str::<Hello>(data)?;
        };

        let login = match &self.session {
            Some(session) => {
                info!("resuming gateway session");
                json!({ "op": 6, "d": { "token": self.config.token, "session_id": session.id, "seq": self.seq } })
            }
            None => {
                info!("identifying");
                self.identify()
            }
        };
        self.send_now(socket, &login).await?;

        let interval = Duration::from_millis(hello.heartbeat_interval);
        let first_beat = Instant::now() + interval.mul_f64(rand::random::<f64>());
        let mut heartbeat = time::interval_at(first_beat, interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut awaiting_ack = false;

        loop {
            let send_slot = self.next_send_slot();
            tokio::select! {
                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        return Err(Error::Zombie);
                    }
                    self.send_now(socket, &json!({ "op": 1, "d": self.seq })).await?;
                    awaiting_ack = true;
                }
                command = self.commands.recv() => match command {
                    Some(command) => self.outbox.push_back(command),
                    None => return Ok(Exit::Shutdown),
                },
                _ = time::sleep_until(send_slot), if !self.outbox.is_empty() => {
                    if let Some(command) = self.outbox.pop_front() {
                        self.send_now(socket, &command).await?;
                    }
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Err(Error::Closed(None));
                    };
                    let message = message?;
                    if let Some(bytes) = frame_bytes(&message, &mut inflater)? {
                        if let Some(exit) = self.handle(bytes, socket, &mut awaiting_ack).await? {
                            return Ok(exit);
                        }
                    }
                }
            }
        }
    }

    async fn handle(
        &mut self,
        bytes: &[u8],
        socket: &mut Socket,
        awaiting_ack: &mut bool,
    ) -> Result<Option<Exit>, Error> {
        let payload: Payload = serde_json::from_slice(bytes)?;
        match payload.op {
            0 => {
                if let Some(seq) = payload.s {
                    self.seq = Some(seq);
                }
                if let (Some(name), Some(data)) = (payload.t, payload.d) {
                    self.dispatch(name, data);
                }
            }
            1 => {
                self.send_now(socket, &json!({ "op": 1, "d": self.seq })).await?;
                *awaiting_ack = true;
            }
            7 => {
                info!("gateway requested a reconnect");
                return Ok(Some(Exit::Reconnect));
            }
            9 => {
                let resumable = payload.d.is_some_and(|d| d.get() == "true");
                info!(resumable, "gateway invalidated the session");
                if !resumable {
                    self.session = None;
                    self.seq = None;
                }
                time::sleep(Duration::from_millis(1000 + rand::random_range(0..4000))).await;
                return Ok(Some(Exit::Reconnect));
            }
            11 => *awaiting_ack = false,
            op => debug!(op, "unhandled gateway opcode"),
        }
        Ok(None)
    }

    fn dispatch(&mut self, name: &str, data: &RawValue) {
        if matches!(name, "READY" | "READY_SUPPLEMENTAL") {
            self.dump(name, data);
        }

        let event = match Event::parse(name, data) {
            Ok(event) => event,
            Err(e) => {
                warn!(event = name, "failed to parse dispatch: {e}");
                self.dump(name, data);
                return;
            }
        };

        match &event {
            Event::Ready(ready) => {
                info!(guilds = ready.guilds.len(), "ready");
                self.session = Some(Session {
                    id: ready.session_id.clone(),
                    resume_url: ready.resume_gateway_url.clone(),
                });
                self.attempt = 0;
                self.set_state(ConnectionState::Connected);
            }
            Event::Resumed => {
                info!("resumed");
                self.attempt = 0;
                self.set_state(ConnectionState::Connected);
            }
            Event::AuthTokenUpdate(update) => self.config.token = update.token.clone(),
            Event::Other(name) => trace!(event = %name, "unhandled dispatch"),
            _ => {}
        }
        let _ = self.notify.send(Notification::Event(event));
    }

    fn identify(&self) -> Value {
        let mut properties = serde_json::to_value(&self.config.properties).unwrap_or_default();
        properties["is_fast_connect"] = false.into();
        properties["gateway_connect_reasons"] = "AppSkeleton".into();
        json!({
            "op": 2,
            "d": {
                "token": self.config.token,
                "capabilities": self.config.capabilities,
                "properties": properties,
                // "unknown" leaves the status the user has set in other clients untouched.
                "presence": { "status": "unknown", "since": 0, "activities": [], "afk": false },
                "compress": false,
                "client_state": { "guild_versions": {} },
            },
        })
    }

    async fn send_now(&mut self, socket: &mut Socket, payload: &Value) -> Result<(), Error> {
        socket.send(Message::text(payload.to_string())).await?;
        self.sent.push_back(Instant::now());
        Ok(())
    }

    /// The earliest instant at which another command fits in the send budget.
    fn next_send_slot(&mut self) -> Instant {
        let now = Instant::now();
        while self.sent.front().is_some_and(|&t| now.duration_since(t) >= SEND_WINDOW) {
            self.sent.pop_front();
        }
        match self.sent.front() {
            Some(&oldest) if self.sent.len() >= SEND_BUDGET => oldest + SEND_WINDOW,
            _ => now,
        }
    }

    fn set_state(&self, state: ConnectionState) {
        let _ = self.notify.send(Notification::State(state));
    }

    /// Writes a raw payload to `BARDO_DUMP_DIR` for inspection during development.
    fn dump(&self, name: &str, data: &RawValue) {
        let Some(dir) = &self.dump_dir else { return };
        let path = dir.join(format!("{name}.json"));
        if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, data.get())) {
            warn!("failed to dump {name}: {e}");
        }
    }
}

/// Returns the payload bytes of a text frame, or of a binary frame that completes a message.
fn frame_bytes<'a>(message: &'a Message, inflater: &'a mut Inflater) -> Result<Option<&'a [u8]>, Error> {
    match message {
        Message::Text(text) => Ok(Some(text.as_bytes())),
        Message::Binary(data) => Ok(inflater.push(data)?),
        Message::Close(frame) => Err(Error::Closed(frame.as_ref().map(|f| u16::from(f.code)))),
        _ => Ok(None),
    }
}

/// Exponential backoff with jitter: ~1 s, 2 s, 4 s … capped at 60 s.
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(Duration::from_secs(60));
    base.mul_f64(0.5 + rand::random::<f64>() * 0.5)
}
