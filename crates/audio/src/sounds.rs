//! Short synthesized sounds: cues for mute and deafen (so a hotkey pressed mid-game is confirmed
//! without looking) and the ringtone for incoming calls.

use std::{
    f32::consts::TAU,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tracing::warn;

use crate::{Playback, SAMPLE_RATE, STEREO_FRAME_LEN};

const VOLUME: f32 = 0.12;
/// Silence after each ring and each ringback.
const RING_PAUSE: f32 = 1.4;
const RINGBACK_PAUSE: f32 = 2.2;
/// Silence left after a cue so the device plays it out.
const TAIL: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// Our own mute and deafen, also when a moderator sets them.
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    /// We joined a channel or call, left it, were disconnected, or were moved by someone else.
    Join,
    Leave,
    Disconnected,
    Moved,
    /// Someone joined or left our channel or call.
    UserJoin,
    UserLeave,
    /// We are alone in a call that had others, or nobody answered.
    CallEnded,
    PushToTalkOn,
    PushToTalkOff,
}

impl Sound {
    pub const ALL: [Sound; 13] = [
        Sound::Mute,
        Sound::Unmute,
        Sound::Deafen,
        Sound::Undeafen,
        Sound::Join,
        Sound::Leave,
        Sound::Disconnected,
        Sound::Moved,
        Sound::UserJoin,
        Sound::UserLeave,
        Sound::CallEnded,
        Sound::PushToTalkOn,
        Sound::PushToTalkOff,
    ];

    /// Direct feedback on our own mute, deafen and push-to-talk (also when a moderator changes
    /// them): it plays while deafened, and without waiting its turn.
    pub fn is_feedback(self) -> bool {
        matches!(
            self,
            Sound::Mute | Sound::Unmute | Sound::Deafen | Sound::Undeafen | Sound::PushToTalkOn | Sound::PushToTalkOff
        )
    }

    fn notes(self) -> &'static [Note] {
        // Falling for "off" and "out", rising for "on" and "in"; deafen is a fifth lower than
        // mute. Our own comings and goings are three-note arpeggios; other people's are a quick
        // pair an octave up, so they stay out of the way.
        match self {
            Sound::Mute => &[Note(880.0, 0.0, 0.07), Note(660.0, 0.075, 0.1)],
            Sound::Unmute => &[Note(660.0, 0.0, 0.07), Note(880.0, 0.075, 0.1)],
            Sound::Deafen => &[Note(587.0, 0.0, 0.07), Note(440.0, 0.075, 0.12)],
            Sound::Undeafen => &[Note(440.0, 0.0, 0.07), Note(587.0, 0.075, 0.12)],
            Sound::Join => &[Note(523.0, 0.0, 0.06), Note(659.0, 0.065, 0.06), Note(784.0, 0.13, 0.12)],
            Sound::Leave => &[Note(784.0, 0.0, 0.06), Note(659.0, 0.065, 0.06), Note(523.0, 0.13, 0.12)],
            Sound::Disconnected => &[Note(587.0, 0.0, 0.08), Note(466.0, 0.085, 0.08), Note(349.0, 0.17, 0.16)],
            Sound::Moved => &[Note(659.0, 0.0, 0.06), Note(988.0, 0.065, 0.06), Note(659.0, 0.13, 0.1)],
            Sound::UserJoin => &[Note(988.0, 0.0, 0.05), Note(1319.0, 0.055, 0.08)],
            Sound::UserLeave => &[Note(1319.0, 0.0, 0.05), Note(988.0, 0.055, 0.08)],
            Sound::CallEnded => &[Note(494.0, 0.0, 0.11), Note(392.0, 0.14, 0.18)],
            Sound::PushToTalkOn => &[Note(1175.0, 0.0, 0.035)],
            Sound::PushToTalkOff => &[Note(880.0, 0.0, 0.035)],
        }
    }

    /// How long the cue lasts, with a short tail of silence.
    fn seconds(self) -> f32 {
        self.notes()
            .iter()
            .map(|&Note(_, start, length)| start + length)
            .fold(0.0, f32::max)
            + TAIL
    }
}

/// A note: frequency in Hz, start and length in seconds.
struct Note(f32, f32, f32);

const RING: &[Note] = &[Note(659.0, 0.0, 0.16), Note(988.0, 0.18, 0.32)];
/// What we hear while the others are being rung: two soft, low pulses.
const RINGBACK: &[Note] = &[Note(440.0, 0.0, 0.3), Note(440.0, 0.4, 0.4)];

/// Renders notes to interleaved stereo, each with a short attack and an exponential decay.
fn render(notes: &[Note], seconds: f32) -> Vec<f32> {
    let frames = (seconds * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0; frames * 2];
    for &Note(frequency, start, length) in notes {
        let first = (start * SAMPLE_RATE as f32) as usize;
        let count = ((length * SAMPLE_RATE as f32) as usize).min(frames.saturating_sub(first));
        for i in 0..count {
            let t = i as f32 / SAMPLE_RATE as f32;
            let attack = (t / 0.005).min(1.0);
            let decay = (-t * 6.0 / length).exp();
            // A touch of the octave makes it less of a test tone.
            let wave = (TAU * frequency * t).sin() + 0.25 * (TAU * 2.0 * frequency * t).sin();
            let sample = wave * attack * decay * VOLUME;
            let index = (first + i) * 2;
            samples[index] += sample;
            samples[index + 1] += sample;
        }
    }
    samples
}

/// Turns rendered samples down (or up) by `volume`.
fn scaled(mut samples: Vec<f32>, volume: f32) -> Vec<f32> {
    let volume = volume.clamp(0.0, 1.0);
    if volume != 1.0 {
        samples.iter_mut().for_each(|sample| *sample *= volume);
    }
    samples
}

/// Feeds `samples` to the device in 20 ms frames, paced in real time, while `keep_going` says so.
/// Returns whether it played to the end.
pub fn play_paced(
    sink: &mut crate::PlaybackSink,
    samples: &[f32],
    start: Instant,
    keep_going: impl Fn() -> bool,
) -> bool {
    for (index, frame) in samples.chunks(STEREO_FRAME_LEN).enumerate() {
        if !keep_going() {
            return false;
        }
        sink.push(frame);
        let next = start + Duration::from_millis(20 * (index as u64 + 1));
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    true
}

/// Plays a cue on the output device with `device_id` (or the default one), in the background,
/// at `volume` (1.0 as made).
pub fn play(sound: Sound, device_id: Option<String>, volume: f32) {
    if volume <= 0.0 {
        return;
    }
    let spawned = thread::Builder::new().name("bardo-sound".into()).spawn(move || {
        let (_playback, mut sink) = match Playback::start(device_id.as_deref(), || {}) {
            Ok(playback) => playback,
            Err(e) => return warn!("failed to open the output for a sound: {e}"),
        };
        let samples = scaled(render(sound.notes(), sound.seconds()), volume);
        play_paced(&mut sink, &samples, Instant::now(), || true);
        // Let the device drain what is queued.
        thread::sleep(Duration::from_millis(150));
    });
    if let Err(e) = spawned {
        warn!("failed to start a sound: {e}");
    }
}

/// Rings until dropped.
pub struct Ringer {
    stop: Arc<AtomicBool>,
}

impl Ringer {
    /// The ringtone of a call that is calling us, at `volume` (1.0 as made).
    pub fn start(device_id: Option<String>, volume: f32) -> Self {
        Self::repeat(scaled(render(RING, 0.5 + RING_PAUSE), volume), device_id)
    }

    /// The ringback of a call we are in while the others are being rung, at `volume`.
    pub fn ringback(device_id: Option<String>, volume: f32) -> Self {
        Self::repeat(scaled(render(RINGBACK, 0.8 + RINGBACK_PAUSE), volume), device_id)
    }

    fn repeat(samples: Vec<f32>, device_id: Option<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let spawned = thread::Builder::new().name("bardo-ringer".into()).spawn(move || {
            let (_playback, mut sink) = match Playback::start(device_id.as_deref(), || {}) {
                Ok(playback) => playback,
                Err(e) => return warn!("failed to open the output for the ringtone: {e}"),
            };
            while play_paced(&mut sink, &samples, Instant::now(), || !stopped.load(Ordering::Relaxed)) {}
        });
        if let Err(e) = spawned {
            warn!("failed to start the ringtone: {e}");
        }
        Self { stop }
    }
}

impl Drop for Ringer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_quiet_and_end_in_silence() {
        for sound in Sound::ALL {
            assert!(sound.seconds() <= 0.4, "{sound:?} lasts {}", sound.seconds());
            let samples = render(sound.notes(), sound.seconds());
            assert_eq!(samples.len(), (sound.seconds() * SAMPLE_RATE as f32) as usize * 2);
            let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert!(peak > 0.05 && peak <= VOLUME * 1.25, "{sound:?} peaks at {peak}");
            assert!(samples[samples.len() - 2].abs() < 1e-3);
        }
    }

    #[test]
    fn volume_scales_the_samples() {
        let samples = render(Sound::Join.notes(), Sound::Join.seconds());
        let peak = |samples: &[f32]| samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let half = scaled(samples.clone(), 0.5);
        assert!((peak(&half) - peak(&samples) / 2.0).abs() < 1e-6);
        assert_eq!(scaled(samples.clone(), 1.0), samples);
        assert_eq!(peak(&scaled(samples.clone(), 0.0)), 0.0);
        // Never louder than made.
        assert_eq!(scaled(samples.clone(), 3.0), samples);
    }

    #[test]
    fn notes_do_not_overlap() {
        for sound in Sound::ALL {
            for pair in sound.notes().windows(2) {
                let (Note(_, start, length), Note(_, next, _)) = (&pair[0], &pair[1]);
                assert!(start + length <= *next, "{sound:?}");
            }
        }
    }

    #[test]
    fn sounds_are_distinct() {
        let pitches = |sound: Sound| sound.notes().iter().map(|note| note.0 as u32).collect::<Vec<_>>();
        for (i, &a) in Sound::ALL.iter().enumerate() {
            for &b in &Sound::ALL[i + 1..] {
                assert_ne!(pitches(a), pitches(b), "{a:?} and {b:?} sound alike");
            }
        }
    }

    #[test]
    fn the_ring_and_the_ringback_have_a_pause() {
        let ring = render(RING, 0.5 + RING_PAUSE);
        assert!(ring[SAMPLE_RATE as usize * 2..].iter().all(|s| *s == 0.0));
        let ringback = render(RINGBACK, 0.8 + RINGBACK_PAUSE);
        assert!(ringback[(0.8 * SAMPLE_RATE as f32) as usize * 2..].iter().all(|s| *s == 0.0));
    }
}
