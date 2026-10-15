//! When a sound may play: not while deafened, and not on top of the previous one.

use std::time::{Duration, Instant};

use bardo_core::Sound;

/// Sounds closer together than this would blur into each other; the later ones are dropped.
const MIN_GAP: Duration = Duration::from_millis(150);

#[derive(Debug, Default)]
pub struct SoundGate {
    last: Option<Instant>,
}

impl SoundGate {
    /// Whether `sound` plays now. Feedback on our own mute, deafen and push-to-talk always does:
    /// it answers a key we just pressed.
    pub fn admit(&mut self, sound: Sound, deaf: bool, now: Instant) -> bool {
        if sound.is_feedback() {
            return true;
        }
        if deaf || self.last.is_some_and(|last| now.duration_since(last) < MIN_GAP) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deafened_hears_only_feedback() {
        let mut gate = SoundGate::default();
        let now = Instant::now();
        assert!(!gate.admit(Sound::UserJoin, true, now));
        assert!(!gate.admit(Sound::Leave, true, now));
        assert!(gate.admit(Sound::Undeafen, true, now));
        assert!(gate.admit(Sound::Mute, true, now));
    }

    #[test]
    fn sounds_in_a_burst_play_once() {
        let mut gate = SoundGate::default();
        let now = Instant::now();
        assert!(gate.admit(Sound::UserJoin, false, now));
        assert!(!gate.admit(Sound::UserJoin, false, now + Duration::from_millis(40)));
        assert!(!gate.admit(Sound::UserLeave, false, now + Duration::from_millis(149)));
        // Feedback is not held back, and does not hold others back.
        assert!(gate.admit(Sound::Unmute, false, now + Duration::from_millis(60)));
        assert!(gate.admit(Sound::UserJoin, false, now + MIN_GAP));
    }
}
