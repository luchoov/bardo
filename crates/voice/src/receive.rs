//! Receive path: mixes everyone's decoded audio with their gain and tracks who is speaking.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bardo_audio::{Mixer, PlaybackSink, rms};
use bardo_gateway::Id;
use songbird::{Event, EventContext, EventHandler};
use tracing::{debug, info, warn};

use crate::{EventCallback, VoiceEvent};

/// Decoded audio quieter than this does not count as speaking (about -46 dBFS).
const SPEAKING_LEVEL: f32 = 0.005;
/// How long someone stays marked as speaking after their last audible frame, to avoid flicker.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);

pub(crate) struct Receiver {
    pub(crate) sink: Option<PlaybackSink>,
    mixer: Mixer,
    /// RTP SSRC → user, learned from speaking updates.
    users: HashMap<u32, Id>,
    gains: HashMap<Id, f32>,
    pub(crate) deafened: bool,
    last_heard: HashMap<Id, Instant>,
    speaking: Vec<Id>,
}

impl Receiver {
    pub(crate) fn new(sink: Option<PlaybackSink>) -> Self {
        Self {
            sink,
            mixer: Mixer::default(),
            users: HashMap::new(),
            gains: HashMap::new(),
            deafened: false,
            last_heard: HashMap::new(),
            speaking: Vec::new(),
        }
    }

    pub(crate) fn set_gain(&mut self, user_id: Id, gain: f32) {
        self.gains.insert(user_id, gain.clamp(0.0, 2.0));
    }

    /// Gain on everyone at once, on top of their own.
    pub(crate) fn set_output_gain(&mut self, gain: f32) {
        self.mixer.set_gain(gain);
    }

    fn map_ssrc(&mut self, ssrc: u32, user_id: Id) {
        if self.users.insert(ssrc, user_id) != Some(user_id) {
            debug!(ssrc, %user_id, "mapped SSRC");
        }
    }

    fn forget_user(&mut self, user_id: Id) {
        self.users.retain(|_, user| *user != user_id);
        self.last_heard.remove(&user_id);
    }

    /// Handles one 20 ms tick of decoded audio per SSRC. Returns the new speaking set if it changed.
    fn tick<'a>(&mut self, frames: impl IntoIterator<Item = (u32, &'a [i16])>, now: Instant) -> Option<Vec<Id>> {
        self.mixer.clear();
        for (ssrc, samples) in frames {
            let user_id = self.users.get(&ssrc).copied();
            if let Some(user_id) = user_id
                && rms(samples) >= SPEAKING_LEVEL
            {
                self.last_heard.insert(user_id, now);
            }
            let gain = user_id.and_then(|id| self.gains.get(&id)).copied().unwrap_or(1.0);
            if !self.deafened && gain > 0.0 {
                self.mixer.add(samples, gain);
            }
        }
        // Keep feeding the device even in silence, so its buffer stays primed.
        if let Some(sink) = &mut self.sink {
            sink.push(self.mixer.finish());
        }

        self.last_heard.retain(|_, heard| now.duration_since(*heard) < SPEAKING_HOLD);
        let mut speaking: Vec<Id> = self.last_heard.keys().copied().collect();
        speaking.sort_unstable();
        (speaking != self.speaking).then(|| {
            self.speaking = speaking.clone();
            speaking
        })
    }
}

#[derive(Clone)]
pub(crate) struct Handler {
    receiver: Arc<Mutex<Receiver>>,
    on_event: EventCallback,
}

impl Handler {
    pub(crate) fn new(receiver: Arc<Mutex<Receiver>>, on_event: EventCallback) -> Self {
        Self { receiver, on_event }
    }

    fn receiver(&self) -> std::sync::MutexGuard<'_, Receiver> {
        self.receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick) => {
                let frames = tick
                    .speaking
                    .iter()
                    .filter_map(|(&ssrc, data)| Some((ssrc, data.decoded_voice.as_deref()?)));
                let changed = self.receiver().tick(frames, Instant::now());
                if let Some(speaking) = changed {
                    (self.on_event)(VoiceEvent::Speaking(speaking));
                }
            }
            EventContext::SpeakingStateUpdate(update) => {
                if let Some(user_id) = update.user_id {
                    self.receiver().map_ssrc(update.ssrc, Id(user_id.0));
                }
            }
            EventContext::ClientDisconnect(disconnect) => {
                self.receiver().forget_user(Id(disconnect.user_id.0));
            }
            EventContext::DriverConnect(data) => {
                info!(ssrc = data.ssrc, server = %data.server, "voice connected");
                (self.on_event)(VoiceEvent::Connected);
            }
            EventContext::DriverReconnect(_) => {
                info!("voice reconnected");
                (self.on_event)(VoiceEvent::Reconnected);
            }
            EventContext::DriverDisconnect(data) => {
                let reason = data.reason.map(|reason| format!("{reason:?}"));
                match &reason {
                    Some(reason) => warn!(kind = ?data.kind, reason, "voice disconnected"),
                    None => info!("voice disconnected"),
                }
                (self.on_event)(VoiceEvent::Disconnected { reason });
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(receiver: &mut Receiver, speakers: &[(u32, i16)], now: Instant) -> Option<Vec<Id>> {
        let frames: Vec<(u32, Vec<i16>)> = speakers.iter().map(|&(ssrc, level)| (ssrc, vec![level; 1920])).collect();
        receiver.tick(frames.iter().map(|(ssrc, samples)| (*ssrc, samples.as_slice())), now)
    }

    #[test]
    fn reports_speaking_changes_with_hold() {
        let mut receiver = Receiver::new(None);
        receiver.map_ssrc(1, Id(10));
        receiver.map_ssrc(2, Id(20));
        let start = Instant::now();

        assert_eq!(tick(&mut receiver, &[(1, 8000), (2, 3)], start), Some(vec![Id(10)]));
        // Unchanged: nothing to report.
        assert_eq!(tick(&mut receiver, &[(1, 8000)], start + Duration::from_millis(20)), None);
        // Silent, but still within the hold time.
        assert_eq!(tick(&mut receiver, &[], start + Duration::from_millis(200)), None);
        assert_eq!(tick(&mut receiver, &[], start + Duration::from_millis(400)), Some(vec![]));
    }

    #[test]
    fn disconnect_forgets_the_user() {
        let mut receiver = Receiver::new(None);
        receiver.map_ssrc(1, Id(10));
        let now = Instant::now();
        assert_eq!(tick(&mut receiver, &[(1, 8000)], now), Some(vec![Id(10)]));
        receiver.forget_user(Id(10));
        assert_eq!(tick(&mut receiver, &[(1, 8000)], now), Some(vec![]));
    }

    #[test]
    fn gains_are_clamped() {
        let mut receiver = Receiver::new(None);
        receiver.set_gain(Id(10), 5.0);
        assert_eq!(receiver.gains[&Id(10)], 2.0);
    }
}
