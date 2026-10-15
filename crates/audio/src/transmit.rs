//! Microphone processing before sending: noise suppression, level metering and the transmit gate
//! (voice activity or push-to-talk).

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use nnnoiseless::DenoiseState;

use crate::{FRAME_LEN, mixer::rms_f32};

/// Frames the gate stays open after voice activity stops, so word endings are not clipped.
const VAD_HANGOVER: u32 = 15;
/// Frames sent after the push-to-talk key is released.
const PTT_RELEASE: u32 = 2;
pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;
/// The level reported for digital silence.
pub const SILENCE_DB: f32 = -100.0;

/// Transmit settings and live state, shared between the UI, the push-to-talk key poller and the
/// capture thread. Everything is atomic: the capture thread never waits.
pub struct TransmitControl {
    push_to_talk: AtomicBool,
    ptt_pressed: AtomicBool,
    /// Voice activity threshold in dBFS (`f32` bits).
    threshold: AtomicU32,
    muted: AtomicBool,
    noise_suppression: AtomicBool,
    /// Level of the last captured frame in dBFS (`f32` bits), for the input meter.
    level: AtomicU32,
    transmitting: AtomicBool,
}

impl Default for TransmitControl {
    fn default() -> Self {
        Self {
            push_to_talk: AtomicBool::new(false),
            ptt_pressed: AtomicBool::new(false),
            threshold: AtomicU32::new(DEFAULT_THRESHOLD_DB.to_bits()),
            muted: AtomicBool::new(false),
            noise_suppression: AtomicBool::new(true),
            level: AtomicU32::new(SILENCE_DB.to_bits()),
            transmitting: AtomicBool::new(false),
        }
    }
}

impl TransmitControl {
    pub fn set_push_to_talk(&self, enabled: bool) {
        self.push_to_talk.store(enabled, Ordering::Relaxed);
    }

    pub fn push_to_talk(&self) -> bool {
        self.push_to_talk.load(Ordering::Relaxed)
    }

    pub fn set_ptt_pressed(&self, pressed: bool) {
        self.ptt_pressed.store(pressed, Ordering::Relaxed);
    }

    pub fn set_threshold_db(&self, threshold: f32) {
        self.threshold.store(threshold.to_bits(), Ordering::Relaxed);
    }

    pub fn threshold_db(&self) -> f32 {
        f32::from_bits(self.threshold.load(Ordering::Relaxed))
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_noise_suppression(&self, enabled: bool) {
        self.noise_suppression.store(enabled, Ordering::Relaxed);
    }

    pub fn noise_suppression(&self) -> bool {
        self.noise_suppression.load(Ordering::Relaxed)
    }

    /// The input level in dBFS, after noise suppression.
    pub fn level_db(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Whether the microphone is being sent right now.
    pub fn transmitting(&self) -> bool {
        self.transmitting.load(Ordering::Relaxed)
    }
}

/// Runs on the capture thread: cleans up each frame and decides whether to send it.
pub struct Transmitter {
    control: Arc<TransmitControl>,
    denoiser: Box<DenoiseState<'static>>,
    scratch: [f32; DenoiseState::FRAME_SIZE],
    gate: Gate,
}

impl Transmitter {
    pub fn new(control: Arc<TransmitControl>) -> Self {
        Self { control, denoiser: DenoiseState::new(), scratch: [0.0; DenoiseState::FRAME_SIZE], gate: Gate::default() }
    }

    /// Processes one 20 ms mono frame in place. Returns whether it should be sent.
    pub fn process(&mut self, frame: &mut [f32]) -> bool {
        debug_assert_eq!(frame.len(), FRAME_LEN);
        if self.control.noise_suppression() {
            self.denoise(frame);
        }
        let control = &self.control;
        let level = level_db(frame);
        control.level.store(level.to_bits(), Ordering::Relaxed);

        let open = if control.muted.load(Ordering::Relaxed) {
            self.gate.close()
        } else if control.push_to_talk() {
            self.gate.update(control.ptt_pressed.load(Ordering::Relaxed), PTT_RELEASE)
        } else {
            self.gate.update(level >= control.threshold_db(), VAD_HANGOVER)
        };
        control.transmitting.store(open, Ordering::Relaxed);
        open
    }

    /// RNNoise works on 10 ms frames of 16-bit-range samples.
    fn denoise(&mut self, frame: &mut [f32]) {
        for chunk in frame.as_chunks_mut::<{ DenoiseState::FRAME_SIZE }>().0 {
            for (scaled, &sample) in self.scratch.iter_mut().zip(chunk.iter()) {
                *scaled = sample * 32768.0;
            }
            self.denoiser.process_frame(chunk.as_mut_slice(), &self.scratch);
            for sample in chunk.iter_mut() {
                *sample /= 32768.0;
            }
        }
    }
}

/// Level of a frame in dBFS.
pub fn level_db(frame: &[f32]) -> f32 {
    let rms = rms_f32(frame);
    if rms <= 1e-5 { SILENCE_DB } else { 20.0 * rms.log10() }
}

/// Opens while its input is active and stays open for a few frames afterwards.
#[derive(Default)]
struct Gate {
    open: bool,
    hold: u32,
}

impl Gate {
    fn update(&mut self, active: bool, hangover: u32) -> bool {
        if active {
            self.open = true;
            self.hold = hangover;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            self.open = false;
        }
        self.open
    }

    fn close(&mut self) -> bool {
        self.open = false;
        self.hold = 0;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transmitter() -> (Arc<TransmitControl>, Transmitter) {
        let control = Arc::new(TransmitControl::default());
        control.set_noise_suppression(false);
        (control.clone(), Transmitter::new(control))
    }

    fn frame(amplitude: f32) -> Vec<f32> {
        (0..FRAME_LEN).map(|i| if i % 2 == 0 { amplitude } else { -amplitude }).collect()
    }

    #[test]
    fn voice_activity_opens_above_the_threshold_and_holds() {
        let (control, mut transmitter) = transmitter();
        control.set_threshold_db(-30.0);
        assert!(!transmitter.process(&mut frame(0.001)));
        assert!(transmitter.process(&mut frame(0.1)));
        assert!(control.transmitting());
        assert!((control.level_db() - -20.0).abs() < 0.1);
        for _ in 0..VAD_HANGOVER {
            assert!(transmitter.process(&mut frame(0.0)));
        }
        assert!(!transmitter.process(&mut frame(0.0)));
        assert_eq!(control.level_db(), SILENCE_DB);
    }

    #[test]
    fn push_to_talk_follows_the_key() {
        let (control, mut transmitter) = transmitter();
        control.set_push_to_talk(true);
        assert!(!transmitter.process(&mut frame(0.5)));
        control.set_ptt_pressed(true);
        assert!(transmitter.process(&mut frame(0.0)));
        control.set_ptt_pressed(false);
        for _ in 0..PTT_RELEASE {
            assert!(transmitter.process(&mut frame(0.0)));
        }
        assert!(!transmitter.process(&mut frame(0.0)));
    }

    #[test]
    fn mute_closes_the_gate_at_once() {
        let (control, mut transmitter) = transmitter();
        control.set_push_to_talk(true);
        control.set_ptt_pressed(true);
        assert!(transmitter.process(&mut frame(0.5)));
        control.set_muted(true);
        assert!(!transmitter.process(&mut frame(0.5)));
        assert!(!control.transmitting());
    }

    #[test]
    fn noise_suppression_keeps_the_frame_size() {
        let (control, mut transmitter) = transmitter();
        control.set_noise_suppression(true);
        let mut samples = frame(0.01);
        transmitter.process(&mut samples);
        assert_eq!(samples.len(), FRAME_LEN);
        assert!(samples.iter().all(|s| s.is_finite()));
    }
}
