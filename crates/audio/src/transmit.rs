//! Microphone processing before sending: noise suppression, input gain, level metering, the
//! transmit gate (voice activity or push-to-talk), automatic gain control, a compressor and a limiter.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use nnnoiseless::DenoiseState;
use tracing::info;

use crate::{FRAME_LEN, SAMPLE_RATE, mixer::rms_f32};

/// Frames the gate stays open after voice activity stops, so word endings are not clipped.
const VAD_HANGOVER: u32 = 15;
/// Frames sent after the push-to-talk key is released.
const PTT_RELEASE: u32 = 2;
pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;
/// The level reported for digital silence.
pub const SILENCE_DB: f32 = -100.0;

/// Average speech level automatic gain control aims for, in dBFS RMS, at 100 % input volume.
const AGC_TARGET_DB: f32 = -16.0;
/// The input volume moves the target within this range, so the slider still does something with
/// automatic gain on.
const AGC_TARGET_RANGE_DB: (f32, f32) = (-40.0, -10.0);
const AGC_MAX_DB: f32 = 24.0;
const AGC_MIN_DB: f32 = -12.0;
/// The speech level is the average power of about the last 1.5 s of speech (75 frames).
const AGC_WINDOW: f32 = 75.0;
/// Gain change per 20 ms frame of speech, towards what the speech level asks for: up at 7.5 dB/s,
/// down at 25 dB/s, so a shout is tamed quickly.
const AGC_RISE_DB: f32 = 0.15;
const AGC_FALL_DB: f32 = 0.5;
/// Frames quieter than this never count as speech for automatic gain control.
const AGC_FLOOR_DB: f32 = -60.0;
/// The compressor turns peaks above this (-6 dBFS) down 4:1, following them within about
/// 1 ms and letting go over about 80 ms.
const COMPRESSOR_THRESHOLD: f32 = 0.5;
const COMPRESSOR_RATIO: f32 = 4.0;
const COMPRESSOR_ATTACK: f32 = 1.0 / (0.001 * SAMPLE_RATE as f32);
const COMPRESSOR_RELEASE: f32 = 1.0 / (0.08 * SAMPLE_RATE as f32);
/// The limiter leaves samples below this untouched and bends louder ones smoothly towards 1.0.
const LIMITER_KNEE: f32 = 0.8;
/// How often the level of the speech being sent is logged, in frames (5 s).
const REPORT_EVERY: u32 = 250;

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
    /// The gain automatic gain control has learned, in dB (`f32` bits). A new capture starts from it.
    auto_gain_db: AtomicU32,
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
            auto_gain_db: AtomicU32::new(0.0f32.to_bits()),
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

    /// Sets where automatic gain control starts on the next capture, e.g. what it learned for this
    /// microphone last time.
    pub fn set_auto_gain_db(&self, gain_db: f32) {
        let gain_db = if gain_db.is_finite() { gain_db.clamp(AGC_MIN_DB, AGC_MAX_DB) } else { 0.0 };
        self.auto_gain_db.store(gain_db.to_bits(), Ordering::Relaxed);
    }

    /// The gain automatic gain control has learned so far, in dB.
    pub fn auto_gain_db(&self) -> f32 {
        f32::from_bits(self.auto_gain_db.load(Ordering::Relaxed))
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
    compressor: Compressor,
    report: SpeechReport,
}

impl Transmitter {
    pub fn new(control: Arc<TransmitControl>) -> Self {
        let auto_gain = AutoGain::new(control.auto_gain_db());
        Self {
            control,
            denoiser: DenoiseState::new(),
            scratch: [0.0; DenoiseState::FRAME_SIZE],
            gate: Gate::default(),
            auto_gain,
            compressor: Compressor::default(),
            report: SpeechReport::default(),
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

        // `active`: speech right now, not the frames the gate is held open for after it.
        let (open, active) = if control.muted.load(Ordering::Relaxed) {
            (self.gate.close(), false)
        } else if control.push_to_talk() {
            let pressed = control.ptt_pressed.load(Ordering::Relaxed);
            (self.gate.update(pressed, PTT_RELEASE), pressed)
        } else {
            let active = level >= control.threshold_db();
            (self.gate.update(active, VAD_HANGOVER), active)
        };
        control.transmitting.store(open, Ordering::Relaxed);
        let speech = open && active && level >= AGC_FLOOR_DB;

        if control.auto_gain() {
            if speech {
                let target = (AGC_TARGET_DB + 20.0 * gain.max(1e-3).log10())
                    .clamp(AGC_TARGET_RANGE_DB.0, AGC_TARGET_RANGE_DB.1);
                self.auto_gain.learn(level, target);
                control.auto_gain_db.store(self.auto_gain.gain_db.to_bits(), Ordering::Relaxed);
            }
            self.auto_gain.apply(frame);
        }
        self.compressor.process(frame);
        limit(frame);
        if let Some(sent) = self.report.add(frame, speech) {
            let auto_gain = format!("{:+.1} dB", self.auto_gain.gain_db);
            info!(level = format!("{sent:.1} dBFS"), auto_gain, "speech sent");
        }
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

/// Steers the average speech level towards a target. Speech level varies a lot from one frame to
/// the next, so the gain follows a smoothed estimate rather than single frames. The estimate only
/// learns from speech: pauses, the frames after it and background noise never pull the gain up.
struct AutoGain {
    gain_db: f32,
    /// Average power of recent speech frames, before automatic gain.
    power: f32,
    /// Speech frames averaged so far, up to [`AGC_WINDOW`].
    frames: f32,
}

impl AutoGain {
    fn new(gain_db: f32) -> Self {
        Self { gain_db: gain_db.clamp(AGC_MIN_DB, AGC_MAX_DB), power: 0.0, frames: 0.0 }
    }

    /// Learns from a speech frame at `level` dBFS and moves the gain towards `target_db`.
    fn learn(&mut self, level: f32, target_db: f32) {
        // A plain average until the window is full, then a moving one. Averaging power, not dB,
        // weighs the body of a word more than its quiet ends.
        self.frames = (self.frames + 1.0).min(AGC_WINDOW);
        self.power += (10f32.powf(level / 10.0) - self.power) / self.frames;
        let speech_db = 10.0 * self.power.log10();
        let wanted = (target_db - speech_db).clamp(AGC_MIN_DB, AGC_MAX_DB);
        self.gain_db += (wanted - self.gain_db).clamp(-AGC_FALL_DB, AGC_RISE_DB);
    }

    fn apply(&self, frame: &mut [f32]) {
        if self.gain_db != 0.0 {
            let gain = 10f32.powf(self.gain_db / 20.0);
            frame.iter_mut().for_each(|sample| *sample *= gain);
        }
    }
}

/// A fast peak compressor, so the limiter is left with only the rare peak to bend.
#[derive(Default)]
struct Compressor {
    envelope: f32,
}

impl Compressor {
    fn process(&mut self, frame: &mut [f32]) {
        for sample in frame {
            let magnitude = sample.abs();
            let speed = if magnitude > self.envelope { COMPRESSOR_ATTACK } else { COMPRESSOR_RELEASE };
            self.envelope += (magnitude - self.envelope) * speed;
            if self.envelope > COMPRESSOR_THRESHOLD {
                *sample *= (self.envelope / COMPRESSOR_THRESHOLD).powf(1.0 / COMPRESSOR_RATIO - 1.0);
            }
        }
    }
}

/// Averages the level of the speech being sent, to log it every [`REPORT_EVERY`] frames.
#[derive(Default)]
struct SpeechReport {
    frames: u32,
    speech_frames: u32,
    power: f32,
}

impl SpeechReport {
    /// Adds a processed frame. Returns the average speech level in dBFS when it is time to log it
    /// and there was speech.
    fn add(&mut self, frame: &[f32], speech: bool) -> Option<f32> {
        if speech {
            self.speech_frames += 1;
            self.power += rms_f32(frame).powi(2);
        }
        self.frames += 1;
        if self.frames < REPORT_EVERY {
            return None;
        }
        let average = self.power / self.speech_frames.max(1) as f32;
        let report = (self.speech_frames > 0).then(|| 10.0 * average.max(1e-10).log10());
        *self = Self::default();
        report
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
    fn the_compressor_and_limiter_keep_samples_in_range() {
        let (control, mut transmitter) = transmitter();
        control.set_input_gain(4.0);
        for _ in 0..5 {
            let mut samples = frame(0.5);
            transmitter.process(&mut samples);
            assert!(samples.iter().all(|s| s.abs() <= 1.0));
        }
        // Peaks 12 dB over the threshold: once the compressor has caught up, the limiter has nothing
        // left to do.
        let mut samples = frame(0.5);
        transmitter.process(&mut samples);
        assert!(samples.iter().all(|s| s.abs() < LIMITER_KNEE), "{}", samples[0]);

        let mut quiet = frame(0.45);
        limit(&mut quiet);
        assert_eq!(quiet[0], 0.45);
        let mut compressor = Compressor::default();
        compressor.process(&mut quiet);
        assert_eq!(quiet[0], 0.45);
    }

    /// Speech-like test signal: syllables of a voiced tone at levels that vary from one to the
    /// next, with short pauses, from a fixed pseudo-random sequence.
    struct Speech {
        seed: u32,
        phase: usize,
        /// Frames left in the current syllable or pause, and its level in dBFS.
        left: u32,
        level: f32,
        talking: bool,
        /// dBFS range of the syllables.
        range: (f32, f32),
    }

    impl Speech {
        fn new(range: (f32, f32)) -> Self {
            Self { seed: 7, phase: 0, left: 0, level: SILENCE_DB, talking: false, range }
        }

        fn random(&mut self) -> f32 {
            self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.seed >> 8) as f32 / (1 << 24) as f32
        }

        /// The next frame, and whether it is part of a syllable.
        fn next(&mut self) -> (Vec<f32>, bool) {
            if self.left == 0 {
                self.talking = !self.talking;
                let (frames, spread) = if self.talking { (8.0, 12.0) } else { (4.0, 8.0) };
                self.left = (frames + self.random() * spread) as u32;
                let (low, high) = self.range;
                self.level = if self.talking { low + self.random() * (high - low) } else { -80.0 };
            }
            self.left -= 1;
            // A 180 Hz tone with two harmonics, scaled to the level (RMS).
            let peak = 10f32.powf(self.level / 20.0) * 2f32.sqrt() / 1.15;
            let frame = (0..FRAME_LEN)
                .map(|i| {
                    let t = (self.phase + i) as f32 / SAMPLE_RATE as f32;
                    let w = std::f32::consts::TAU * 180.0 * t;
                    peak * (w.sin() + 0.5 * (2.0 * w).sin() + 0.3 * (3.0 * w).sin())
                })
                .collect();
            self.phase += FRAME_LEN;
            (frame, self.talking)
        }
    }

    /// Runs `seconds` of `speech` through the transmitter. Returns the average level of the
    /// syllables sent, in dBFS.
    fn talk(transmitter: &mut Transmitter, speech: &mut Speech, seconds: f32) -> f32 {
        let (mut power, mut count) = (0.0, 0);
        for _ in 0..(seconds * 50.0) as usize {
            let (mut samples, talking) = speech.next();
            transmitter.process(&mut samples);
            assert!(samples.iter().all(|s| s.abs() <= 1.0));
            if talking {
                power += rms_f32(&samples).powi(2);
                count += 1;
            }
        }
        10.0 * (power / count as f32).log10()
    }

    fn auto_gain() -> (Arc<TransmitControl>, Transmitter) {
        let (control, _) = transmitter();
        control.set_auto_gain(true);
        control.set_threshold_db(-55.0);
        (control.clone(), Transmitter::new(control))
    }

    #[test]
    fn auto_gain_brings_varying_speech_to_the_target() {
        let (control, mut transmitter) = auto_gain();
        // Quiet speech: syllables between -44 and -28 dBFS, about -32 dBFS on average.
        let mut speech = Speech::new((-44.0, -28.0));
        talk(&mut transmitter, &mut speech, 4.0);
        let sent = talk(&mut transmitter, &mut speech, 4.0);
        assert!((sent - AGC_TARGET_DB).abs() < 1.5, "sent {sent} dBFS");
        assert_eq!(control.auto_gain_db(), transmitter.auto_gain.gain_db);

        // Loud speech comes down to the same level.
        let (_, mut transmitter) = auto_gain();
        let mut speech = Speech::new((-12.0, -2.0));
        talk(&mut transmitter, &mut speech, 4.0);
        let sent = talk(&mut transmitter, &mut speech, 4.0);
        assert!((sent - AGC_TARGET_DB).abs() < 1.5, "sent {sent} dBFS");
    }

    #[test]
    fn the_input_volume_moves_the_auto_gain_target() {
        let (control, mut transmitter) = auto_gain();
        control.set_input_gain(0.5);
        let mut speech = Speech::new((-44.0, -28.0));
        talk(&mut transmitter, &mut speech, 4.0);
        let sent = talk(&mut transmitter, &mut speech, 4.0);
        assert!((sent - (AGC_TARGET_DB - 6.0)).abs() < 1.5, "sent {sent} dBFS");
    }

    #[test]
    fn auto_gain_does_not_amplify_noise_or_silence() {
        let (control, mut transmitter) = auto_gain();
        // Noise below the floor that still opens the gate.
        control.set_threshold_db(-70.0);
        for _ in 0..500 {
            let mut samples = frame(0.0005);
            assert!(transmitter.process(&mut samples));
            assert!(level_db(&samples) < AGC_FLOOR_DB);
        }
        assert_eq!(transmitter.auto_gain.gain_db, 0.0);

        // After speech, pauses and silence leave the gain where it was.
        let mut speech = Speech::new((-44.0, -28.0));
        talk(&mut transmitter, &mut speech, 3.0);
        let before = transmitter.auto_gain.gain_db;
        for _ in 0..100 {
            transmitter.process(&mut frame(0.0005));
            transmitter.process(&mut frame(0.0));
        }
        assert_eq!(transmitter.auto_gain.gain_db, before);
    }

    #[test]
    fn the_frames_held_open_after_speech_do_not_count() {
        let (_, mut transmitter) = auto_gain();
        transmitter.process(&mut frame(0.02));
        let after_speech = transmitter.auto_gain.gain_db;
        // Quiet frames in the hangover would pull the gain up if they counted.
        for _ in 0..VAD_HANGOVER {
            assert!(transmitter.process(&mut frame(0.0015)));
        }
        assert_eq!(transmitter.auto_gain.gain_db, after_speech);
    }

    #[test]
    fn auto_gain_tames_a_shout_quickly() {
        let (_, mut transmitter) = auto_gain();
        let mut speech = Speech::new((-44.0, -28.0));
        talk(&mut transmitter, &mut speech, 8.0);
        let learned = transmitter.auto_gain.gain_db;
        assert!(learned > 12.0, "{learned}");

        // Shouting at -4 dBFS with all that gain: the compressor and limiter hold the peaks at once,
        // and within half a second the gain has come well down.
        let mut shout = Speech { talking: true, left: 25, level: -4.0, ..Speech::new((-4.0, -4.0)) };
        for _ in 0..25 {
            let (mut samples, _) = shout.next();
            transmitter.process(&mut samples);
            assert!(samples.iter().all(|s| s.abs() <= 1.0));
        }
        assert!(transmitter.auto_gain.gain_db < learned - 10.0, "{}", transmitter.auto_gain.gain_db);
    }

    #[test]
    fn auto_gain_starts_from_what_it_learned() {
        let (control, _) = auto_gain();
        control.set_auto_gain_db(9.0);
        let transmitter = Transmitter::new(control.clone());
        assert_eq!(transmitter.auto_gain.gain_db, 9.0);
        control.set_auto_gain_db(80.0);
        assert_eq!(control.auto_gain_db(), AGC_MAX_DB);
        control.set_auto_gain_db(f32::NAN);
        assert_eq!(control.auto_gain_db(), 0.0);
    }

    #[test]
    fn reports_the_speech_level_every_few_seconds() {
        let mut report = SpeechReport::default();
        for _ in 0..REPORT_EVERY - 1 {
            assert_eq!(report.add(&frame(0.1), true), None);
        }
        let level = report.add(&frame(0.0), false).unwrap();
        assert!((level - -20.0).abs() < 0.1, "{level}");
        // Nothing to say about a stretch without speech.
        for _ in 0..REPORT_EVERY {
            assert_eq!(report.add(&frame(0.1), false), None);
        }
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
