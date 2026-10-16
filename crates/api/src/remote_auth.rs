//! Signing in by scanning a QR code with the Discord app on a phone, through Discord's remote
//! authentication gateway (see docs.discord.food, "Remote Authentication").
//!
//! We send a fresh RSA public key; the gateway proves it can reach us by having us decrypt a nonce,
//! then gives us a fingerprint of the key to show as a QR code. When the phone scans it we get the
//! user's name, and when they confirm, a ticket that the REST API exchanges for the token, encrypted
//! for our key. The token, the ticket and the decrypted payloads are never logged.

use std::{fmt, time::Duration};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL},
};
use bardo_gateway::ClientProperties;
use futures_util::{SinkExt, StreamExt};
use rsa::{
    Oaep, RsaPrivateKey, RsaPublicKey,
    pkcs8::EncodePublicKey,
    rand_core::{OsRng, RngCore},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::{self, Instant};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue, protocol::frame::coding::CloseCode},
};
use tracing::{debug, info, warn};

use crate::{error::Error, http};

const GATEWAY_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
/// What the QR code holds: the fingerprint, for the phone to find this session.
const QR_URL: &str = "https://discord.com/ra/";
const KEY_BITS: usize = 2048;
/// Waits before trying again after a connection problem.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// The gateway closes with this when the session's time is up; a new one starts with a new code.
const CLOSE_TIMED_OUT: u16 = 4003;

/// What the sign-in screen shows as the session goes on.
pub enum RemoteAuthEvent {
    /// A new code to show, as the text of the QR code.
    Code(String),
    /// The code was scanned; the user named here confirms on the phone.
    Scanned { username: String },
    /// Signed in.
    Token(String),
    /// Something went wrong. `retrying` is false when QR sign-in cannot work, e.g. a captcha.
    Problem { message: String, retrying: bool },
}

impl fmt::Debug for RemoteAuthEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteAuthEvent::Code(_) => f.write_str("Code"),
            RemoteAuthEvent::Scanned { .. } => f.write_str("Scanned"),
            RemoteAuthEvent::Token(_) => f.write_str("Token"),
            RemoteAuthEvent::Problem { message, retrying } => {
                f.debug_struct("Problem").field("message", message).field("retrying", retrying).finish()
            }
        }
    }
}

/// Runs remote authentication sessions, one after another as they expire or are canceled, until
/// one signs in or a problem makes it pointless. Returns the token, if any.
pub async fn remote_auth(
    properties: &ClientProperties,
    mut on_event: impl FnMut(RemoteAuthEvent),
) -> Option<String> {
    loop {
        match session(properties, &mut on_event).await {
            Ok(Some(token)) => {
                info!("signed in with a QR code");
                on_event(RemoteAuthEvent::Token(token.clone()));
                return Some(token);
            }
            Ok(None) => debug!("remote auth session over, starting another"),
            Err(Error::CaptchaRequired) => {
                warn!("remote auth login wants a captcha");
                on_event(RemoteAuthEvent::Problem {
                    message: "Discord wants a captcha to sign in this way. Use a token instead.".into(),
                    retrying: false,
                });
                return None;
            }
            Err(e) => {
                warn!("remote auth failed: {e}");
                on_event(RemoteAuthEvent::Problem {
                    message: "Couldn't reach Discord. Trying again…".into(),
                    retrying: true,
                });
                time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// One session: from connecting to the token, or `None` when it ended without one.
async fn session(
    properties: &ClientProperties,
    on_event: &mut impl FnMut(RemoteAuthEvent),
) -> Result<Option<String>, Error> {
    // Generating the key takes a moment; keep it off the runtime's threads.
    let handshake = tokio::task::spawn_blocking(Handshake::new)
        .await
        .map_err(|e| Error::RemoteAuth(e.to_string()))??;

    let mut request = GATEWAY_URL.into_client_request().map_err(|e| Error::RemoteAuth(e.to_string()))?;
    let headers = request.headers_mut();
    if let Ok(user_agent) = HeaderValue::from_str(properties.user_agent()) {
        headers.insert("User-Agent", user_agent);
    }
    headers.insert("Origin", HeaderValue::from_static("https://discord.com"));
    let (mut socket, _) = connect_async(request).await.map_err(|e| Error::RemoteAuth(e.to_string()))?;

    let mut heartbeat: Option<(Duration, Instant)> = None;
    let mut awaiting_ack = false;
    loop {
        let next_beat = heartbeat.map(|(_, at)| at);
        let message = tokio::select! {
            message = socket.next() => message,
            () = sleep_until(next_beat) => {
                if awaiting_ack {
                    return Err(Error::RemoteAuth("no heartbeat acknowledgement".into()));
                }
                send(&mut socket, json!({ "op": "heartbeat" })).await?;
                awaiting_ack = true;
                if let Some((interval, at)) = &mut heartbeat {
                    *at = Instant::now() + *interval;
                }
                continue;
            }
        };
        let text = match message {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(frame))) => {
                let code = frame.as_ref().map(|frame| u16::from(frame.code));
                return match code {
                    // Canceled on the phone, finished without us, or expired: start over.
                    Some(CLOSE_TIMED_OUT) | None => Ok(None),
                    Some(code) if code == u16::from(CloseCode::Normal) => Ok(None),
                    Some(code) => Err(Error::RemoteAuth(format!("closed with {code}"))),
                };
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(Error::RemoteAuth(e.to_string())),
            None => return Ok(None),
        };

        match handshake.handle(&text)? {
            Step::Hello { heartbeat_interval } => {
                // The first beat comes at a random point within the interval.
                let first = heartbeat_interval.mul_f64(f64::from(OsRng.next_u32()) / f64::from(u32::MAX));
                heartbeat = Some((heartbeat_interval, Instant::now() + first));
                send(&mut socket, handshake.init()).await?;
            }
            Step::Reply(reply) => send(&mut socket, reply).await?,
            Step::Code(code) => on_event(RemoteAuthEvent::Code(code)),
            Step::Scanned(user) => on_event(RemoteAuthEvent::Scanned { username: user.username }),
            Step::Login(ticket) => {
                let _ = socket.close(None).await;
                let encrypted = http::remote_auth_login(properties, &ticket).await?;
                return handshake.decrypt_token(&encrypted).map(Some);
            }
            Step::Cancel => return Ok(None),
            Step::HeartbeatAck => awaiting_ack = false,
            Step::Ignore => {}
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn send<S>(socket: &mut S, message: Value) -> Result<(), Error>
where
    S: SinkExt<Message> + Unpin,
    S::Error: fmt::Display,
{
    socket.send(Message::text(message.to_string())).await.map_err(|e| Error::RemoteAuth(e.to_string()))
}

/// The user who scanned the code, from the `pending_ticket` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedUser {
    username: String,
}

/// What to do after a gateway message.
#[derive(Debug, PartialEq)]
enum Step {
    Hello { heartbeat_interval: Duration },
    /// Send this back.
    Reply(Value),
    /// Show this as the QR code.
    Code(String),
    Scanned(ScannedUser),
    /// Exchange this ticket for the token.
    Login(String),
    Cancel,
    HeartbeatAck,
    Ignore,
}

/// The cryptography of a session, separate from the connection so it can be tested.
struct Handshake {
    key: RsaPrivateKey,
    /// The DER-encoded public key (SubjectPublicKeyInfo).
    public_key: Vec<u8>,
}

impl Handshake {
    fn new() -> Result<Self, Error> {
        let key = RsaPrivateKey::new(&mut OsRng, KEY_BITS).map_err(|e| Error::RemoteAuth(e.to_string()))?;
        let public_key = RsaPublicKey::from(&key)
            .to_public_key_der()
            .map_err(|e| Error::RemoteAuth(e.to_string()))?
            .into_vec();
        Ok(Self { key, public_key })
    }

    fn init(&self) -> Value {
        json!({ "op": "init", "encoded_public_key": BASE64.encode(&self.public_key) })
    }

    /// The fingerprint the gateway must announce for our key.
    fn fingerprint(&self) -> String {
        BASE64_URL.encode(Sha256::digest(&self.public_key))
    }

    fn decrypt(&self, base64: &str) -> Result<Vec<u8>, Error> {
        let encrypted = BASE64.decode(base64).map_err(|e| Error::RemoteAuth(e.to_string()))?;
        self.key.decrypt(Oaep::new::<Sha256>(), &encrypted).map_err(|e| Error::RemoteAuth(e.to_string()))
    }

    fn decrypt_token(&self, encrypted_token: &str) -> Result<String, Error> {
        String::from_utf8(self.decrypt(encrypted_token)?).map_err(|_| Error::RemoteAuth("token is not text".into()))
    }

    fn handle(&self, text: &str) -> Result<Step, Error> {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum Incoming {
            Hello { heartbeat_interval: u64 },
            NonceProof { encrypted_nonce: String },
            PendingRemoteInit { fingerprint: String },
            PendingTicket { encrypted_user_payload: String },
            PendingLogin { ticket: String },
            Cancel,
            HeartbeatAck,
            #[serde(other)]
            Other,
        }

        let incoming: Incoming = serde_json::from_str(text).map_err(|e| Error::RemoteAuth(e.to_string()))?;
        Ok(match incoming {
            Incoming::Hello { heartbeat_interval } => {
                Step::Hello { heartbeat_interval: Duration::from_millis(heartbeat_interval) }
            }
            Incoming::NonceProof { encrypted_nonce } => {
                let nonce = self.decrypt(&encrypted_nonce)?;
                Step::Reply(json!({ "op": "nonce_proof", "nonce": BASE64_URL.encode(nonce) }))
            }
            Incoming::PendingRemoteInit { fingerprint } => {
                if fingerprint != self.fingerprint() {
                    return Err(Error::RemoteAuth("the gateway announced another key".into()));
                }
                Step::Code(format!("{QR_URL}{fingerprint}"))
            }
            Incoming::PendingTicket { encrypted_user_payload } => {
                let payload = String::from_utf8(self.decrypt(&encrypted_user_payload)?)
                    .map_err(|_| Error::RemoteAuth("user payload is not text".into()))?;
                Step::Scanned(parse_user(&payload)?)
            }
            Incoming::PendingLogin { ticket } => Step::Login(ticket),
            Incoming::Cancel => Step::Cancel,
            Incoming::HeartbeatAck => Step::HeartbeatAck,
            Incoming::Other => Step::Ignore,
        })
    }
}

/// Parses "id:discriminator:avatar:username". The username comes last and is taken whole.
fn parse_user(payload: &str) -> Result<ScannedUser, Error> {
    let mut parts = payload.splitn(4, ':');
    let (Some(_id), Some(_discriminator), Some(_avatar), Some(username)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::RemoteAuth("unexpected user payload".into()));
    };
    Ok(ScannedUser { username: username.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake() -> Handshake {
        // Key generation is slow unoptimized; one smaller key is enough to test the protocol.
        let key = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
        let public_key = RsaPublicKey::from(&key).to_public_key_der().unwrap().into_vec();
        Handshake { key, public_key }
    }

    fn encrypt_for(handshake: &Handshake, data: &[u8]) -> String {
        let public = RsaPublicKey::from(&handshake.key);
        BASE64.encode(public.encrypt(&mut OsRng, Oaep::new::<Sha256>(), data).unwrap())
    }

    #[test]
    fn walks_through_a_session() {
        let handshake = handshake();
        assert_eq!(
            handshake.handle(r#"{"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 120000}"#).unwrap(),
            Step::Hello { heartbeat_interval: Duration::from_millis(41250) }
        );

        let init = handshake.init();
        assert_eq!(init["op"], "init");
        let sent_key = BASE64.decode(init["encoded_public_key"].as_str().unwrap()).unwrap();
        assert_eq!(sent_key, handshake.public_key);

        // The nonce comes back decrypted, as unpadded base64url.
        let nonce = [0xfb, 0xff, 0x01, 0x02, 0x03];
        let message = json!({ "op": "nonce_proof", "encrypted_nonce": encrypt_for(&handshake, &nonce) });
        let Step::Reply(reply) = handshake.handle(&message.to_string()).unwrap() else { panic!() };
        assert_eq!(reply, json!({ "op": "nonce_proof", "nonce": "-_8BAgM" }));

        let fingerprint = BASE64_URL.encode(Sha256::digest(&handshake.public_key));
        let message = json!({ "op": "pending_remote_init", "fingerprint": fingerprint });
        assert_eq!(handshake.handle(&message.to_string()).unwrap(), Step::Code(format!("{QR_URL}{fingerprint}")));

        let payload = encrypt_for(&handshake, b"123456789:0:a_1b2c:some:user");
        let message = json!({ "op": "pending_ticket", "encrypted_user_payload": payload });
        assert_eq!(
            handshake.handle(&message.to_string()).unwrap(),
            Step::Scanned(ScannedUser { username: "some:user".into() })
        );

        let message = json!({ "op": "pending_login", "ticket": "the-ticket" });
        assert_eq!(handshake.handle(&message.to_string()).unwrap(), Step::Login("the-ticket".into()));
        assert_eq!(handshake.decrypt_token(&encrypt_for(&handshake, b"mfa.token")).unwrap(), "mfa.token");

        assert_eq!(handshake.handle(r#"{"op": "cancel"}"#).unwrap(), Step::Cancel);
        assert_eq!(handshake.handle(r#"{"op": "heartbeat_ack"}"#).unwrap(), Step::HeartbeatAck);
        assert_eq!(handshake.handle(r#"{"op": "something_new", "x": 1}"#).unwrap(), Step::Ignore);
    }

    #[test]
    fn rejects_a_fingerprint_for_another_key() {
        let handshake = handshake();
        let message = json!({ "op": "pending_remote_init", "fingerprint": "c29tZW9uZSBlbHNl" });
        assert!(handshake.handle(&message.to_string()).is_err());
    }

    #[test]
    fn rejects_payloads_it_cannot_read() {
        let handshake = handshake();
        let message = json!({ "op": "nonce_proof", "encrypted_nonce": BASE64.encode([1, 2, 3]) });
        assert!(handshake.handle(&message.to_string()).is_err());
        assert!(parse_user("123:0").is_err());
        assert!(handshake.handle("not json").is_err());
    }

    #[test]
    fn events_do_not_show_secrets() {
        let token = format!("{:?}", RemoteAuthEvent::Token("secret".into()));
        let scanned = format!("{:?}", RemoteAuthEvent::Scanned { username: "someone".into() });
        assert!(!token.contains("secret") && !scanned.contains("someone"));
    }
}

