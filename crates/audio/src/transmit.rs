//! Microphone processing before sending: noise suppression, input gain, level metering, the
//! transmit gate (voice activity or push-to-talk), automatic gain control and a limiter.

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

/// Speech level automatic gain control aims for, in dBFS RMS.
const AGC_TARGET_DB: f32 = -20.0;
const AGC_MAX_DB: f32 = 18.0;
const AGC_MIN_DB: f32 = -12.0;
/// Gain change per 20 ms frame: rises slowly (3 dB/s) and falls fast (20 dB/s), so a shout is
/// tamed quickly but the gain does not pump up between words.
const AGC_RISE_DB: f32 = 0.06;
const AGC_FALL_DB: f32 = 0.4;
/// Frames quieter than this never count as speech for automatic gain control.
const AGC_FLOOR_DB: f32 = -60.0;
/// The limiter leaves samples below this untouched and bends louder ones smoothly towards 1.0.
const LIMITER_KNEE: f32 = 0.8;

/// Transmit settings and live state, shared between the UI, the push-to-talk key poller and the
/// capture thread. Everything is atomic: the capture thread never waits.
pub struct TransmitControl {
    push_to_talk: AtomicBool,
    ptt_pressed: AtomicBool,
    /// Voice activity threshold in dBFS (`f32` bits).
    threshold: AtomicU32,
    muted: AtomicBool,
    noise_suppression: AtomicBool,
    /// Linear input gain (`f32` bits).
    input_gain: AtomicU32,
    auto_gain: AtomicBool,
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
            input_gain: AtomicU32::new(1.0f32.to_bits()),
            auto_gain: AtomicBool::new(true),
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

    /// Sets the linear input gain (1.0 leaves the microphone as it is).
    pub fn set_input_gain(&self, gain: f32) {
        self.input_gain.store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn input_gain(&self) -> f32 {
        f32::from_bits(self.input_gain.load(Ordering::Relaxed))
    }

    pub fn set_auto_gain(&self, enabled: bool) {
        self.auto_gain.store(enabled, Ordering::Relaxed);
    }

    pub fn auto_gain(&self) -> bool {
        self.auto_gain.load(Ordering::Relaxed)
    }

    /// The input level in dBFS, after noise suppression and input gain.
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
    auto_gain: AutoGain,
}

impl Transmitter {
    pub fn new(control: Arc<TransmitControl>) -> Self {
        Self {
            control,
            denoiser: DenoiseState::new(),
            scratch: [0.0; DenoiseState::FRAME_SIZE],
            gate: Gate::default(),
            auto_gain: AutoGain::default(),
        }
    }

    /// Processes one 20 ms mono frame in place. Returns whether it should be sent.
    pub fn process(&mut self, frame: &mut [f32]) -> bool {
        debug_assert_eq!(frame.len(), FRAME_LEN);
        if self.control.noise_suppression() {
            self.denoise(frame);
        }
        let control = &self.control;
        let gain = control.input_gain();
        if gain != 1.0 {
            frame.iter_mut().for_each(|sample| *sample *= gain);
        }
        // The meter and voice activity see the level before automatic gain, so the threshold
        // keeps meaning the same thing while the automatic gain moves.
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

        if control.auto_gain() {
            self.auto_gain.process(frame, level, open && level >= AGC_FLOOR_DB);
        }
        limit(frame);
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

/// Slowly steers the speech level towards [`AGC_TARGET_DB`]. It only learns from frames that are
/// being sent and loud enough to be speech, so background noise is never pumped up.
#[derive(Default)]
struct AutoGain {
    gain_db: f32,
}

impl AutoGain {
    fn process(&mut self, frame: &mut [f32], level: f32, speech: bool) {
        if speech {
            let error = AGC_TARGET_DB - (level + self.gain_db);
            self.gain_db = (self.gain_db + error.clamp(-AGC_FALL_DB, AGC_RISE_DB)).clamp(AGC_MIN_DB, AGC_MAX_DB);
        }
        if self.gain_db != 0.0 {
            let gain = 10f32.powf(self.gain_db / 20.0);
            frame.iter_mut().for_each(|sample| *sample *= gain);
        }
    }
}

/// Soft-clips samples above [`LIMITER_KNEE`] so extra gain never wraps or hard-clips.
fn limit(frame: &mut [f32]) {
    const RANGE: f32 = 1.0 - LIMITER_KNEE;
    for sample in frame {
        let magnitude = sample.abs();
        if magnitude > LIMITER_KNEE {
            let bent = LIMITER_KNEE + RANGE * ((magnitude - LIMITER_KNEE) / RANGE).tanh();
            *sample = bent.copysign(*sample);
        }
    }
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
        control.set_auto_gain(false);
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
    fn input_gain_scales_the_level() {
        let (control, mut transmitter) = transmitter();
        control.set_input_gain(2.0);
        let mut samples = frame(0.1);
        transmitter.process(&mut samples);
        assert!((samples[0] - 0.2).abs() < 1e-6);
        assert!((control.level_db() - -13.98).abs() < 0.1);
    }

    #[test]
    fn the_limiter_keeps_samples_in_range() {
        let (control, mut transmitter) = transmitter();
        control.set_input_gain(10.0);
        let mut samples = frame(0.5);
        transmitter.process(&mut samples);
        assert!(samples.iter().all(|s| s.abs() <= 1.0));
        assert!(samples[0] > 0.95);

        let mut quiet = frame(0.5);
        limit(&mut quiet);
        assert_eq!(quiet[0], 0.5);
    }

    #[test]
    fn auto_gain_raises_quiet_speech_but_not_silence() {
        let (control, mut transmitter) = transmitter();
        control.set_auto_gain(true);
        control.set_threshold_db(-50.0);
        // Five seconds of quiet speech at -40 dBFS.
        for _ in 0..250 {
            transmitter.process(&mut frame(0.01));
        }
        let mut samples = frame(0.01);
        transmitter.process(&mut samples);
        let boosted = level_db(&samples);
        assert!(boosted > -30.0, "speech should be louder, got {boosted} dB");
        // Silence does not move the gain.
        let before = transmitter.auto_gain.gain_db;
        for _ in 0..100 {
            transmitter.process(&mut frame(0.0));
        }
        assert_eq!(transmitter.auto_gain.gain_db, before);
    }

    #[test]
    fn auto_gain_tames_loud_speech_quickly() {
        let (control, mut transmitter) = transmitter();
        control.set_auto_gain(true);
        for _ in 0..25 {
            transmitter.process(&mut frame(0.9));
        }
        assert!(transmitter.auto_gain.gain_db < -5.0);
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
