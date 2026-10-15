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
/// Silence between rings.
const RING_PAUSE: f32 = 1.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Mute,
    Unmute,
    Deafen,
    Undeafen,
}

/// A note: frequency in Hz, start and length in seconds.
struct Note(f32, f32, f32);

impl Sound {
    fn notes(self) -> &'static [Note] {
        // Falling for "off", rising for "on"; deafen is a fifth lower than mute.
        match self {
            Sound::Mute => &[Note(880.0, 0.0, 0.07), Note(660.0, 0.075, 0.1)],
            Sound::Unmute => &[Note(660.0, 0.0, 0.07), Note(880.0, 0.075, 0.1)],
            Sound::Deafen => &[Note(587.0, 0.0, 0.07), Note(440.0, 0.075, 0.12)],
            Sound::Undeafen => &[Note(440.0, 0.0, 0.07), Note(587.0, 0.075, 0.12)],
        }
    }
}

const RING: &[Note] = &[Note(659.0, 0.0, 0.16), Note(988.0, 0.18, 0.32)];

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

/// Feeds `samples` to the device in 20 ms frames, paced in real time, while `keep_going` says so.
fn play_paced(sink: &mut crate::PlaybackSink, samples: &[f32], start: Instant, keep_going: impl Fn() -> bool) -> bool {
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

/// Plays a cue on the output device with `device_id` (or the default one), in the background.
pub fn play(sound: Sound, device_id: Option<String>) {
    let spawned = thread::Builder::new().name("bardo-sound".into()).spawn(move || {
        let (_playback, mut sink) = match Playback::start(device_id.as_deref()) {
            Ok(playback) => playback,
            Err(e) => return warn!("failed to open the output for a sound: {e}"),
        };
        let samples = render(sound.notes(), 0.3);
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
    pub fn start(device_id: Option<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let spawned = thread::Builder::new().name("bardo-ringer".into()).spawn(move || {
            let (_playback, mut sink) = match Playback::start(device_id.as_deref()) {
                Ok(playback) => playback,
                Err(e) => return warn!("failed to open the output for the ringtone: {e}"),
            };
            let ring = render(RING, 0.5 + RING_PAUSE);
            while play_paced(&mut sink, &ring, Instant::now(), || !stopped.load(Ordering::Relaxed)) {}
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
    fn sounds_are_quiet_and_end_in_silence() {
        for sound in [Sound::Mute, Sound::Unmute, Sound::Deafen, Sound::Undeafen] {
            let samples = render(sound.notes(), 0.3);
            assert_eq!(samples.len(), (0.3 * SAMPLE_RATE as f32) as usize * 2);
            let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert!(peak > 0.05 && peak <= VOLUME * 1.25, "{sound:?} peaks at {peak}");
            assert!(samples[samples.len() - 2].abs() < 1e-3);
        }
    }

    #[test]
    fn the_ring_has_a_pause() {
        let ring = render(RING, 0.5 + RING_PAUSE);
        let pause = &ring[SAMPLE_RATE as usize * 2..];
        assert!(pause.iter().all(|s| *s == 0.0));
    }
}
