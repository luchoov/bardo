use crate::{STEREO_FRAME_LEN, limiter::limit};

/// The most the whole mix can be turned up.
pub const MAX_MIX_GAIN: f32 = 2.0;

/// Sums 20 ms stereo frames of decoded voice into one output frame, applying a gain per source
/// and one to the whole mix.
pub struct Mixer {
    frame: Vec<f32>,
    gain: f32,
}

impl Default for Mixer {
    fn default() -> Self {
        Self { frame: vec![0.0; STEREO_FRAME_LEN], gain: 1.0 }
    }
}

impl Mixer {
    /// Starts a new frame of silence.
    pub fn clear(&mut self) {
        self.frame.fill(0.0);
    }

    /// Adds interleaved stereo 16-bit samples at `gain` (1.0 = unchanged).
    pub fn add(&mut self, samples: &[i16], gain: f32) {
        let gain = gain / f32::from(i16::MAX);
        for (out, &sample) in self.frame.iter_mut().zip(samples) {
            *out += f32::from(sample) * gain;
        }
    }

    /// Sets the gain on the whole mix, from 0.0 (silence) to [`MAX_MIX_GAIN`] (1.0 = unchanged).
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = if gain.is_finite() { gain.clamp(0.0, MAX_MIX_GAIN) } else { 1.0 };
    }

    /// The mixed frame at the mix gain, softly limited to [-1, 1].
    pub fn finish(&mut self) -> &[f32] {
        if self.gain != 1.0 {
            let gain = self.gain;
            self.frame.iter_mut().for_each(|sample| *sample *= gain);
        }
        limit(&mut self.frame);
        &self.frame
    }
}

/// Root mean square level of 16-bit samples, from 0.0 (silence) to 1.0 (full scale).
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / samples.len() as f64).sqrt() / f64::from(i16::MAX)) as f32
}

/// Root mean square level of float samples in [-1, 1].
pub fn rms_f32(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_sources_with_their_gain() {
        let mut mixer = Mixer::default();
        mixer.clear();
        let half = i16::MAX / 2;
        mixer.add(&[half; STEREO_FRAME_LEN], 1.0);
        mixer.add(&[half; STEREO_FRAME_LEN], 0.5);
        let frame = mixer.finish();
        assert_eq!(frame.len(), STEREO_FRAME_LEN);
        assert!((frame[0] - 0.75).abs() < 0.001, "{}", frame[0]);
    }

    #[test]
    fn limits_loud_mixes_and_starts_from_silence() {
        let mut mixer = Mixer::default();
        mixer.add(&[i16::MAX; STEREO_FRAME_LEN], 2.0);
        let loud = mixer.finish()[10];
        assert!(loud > 0.99 && loud <= 1.0, "{loud}");
        mixer.clear();
        assert_eq!(mixer.finish()[10], 0.0);
    }

    #[test]
    fn the_mix_gain_turns_everyone_up_without_clipping() {
        let mut mixer = Mixer::default();
        mixer.set_gain(1.5);
        mixer.add(&[i16::MAX / 4; STEREO_FRAME_LEN], 1.0);
        assert!((mixer.finish()[0] - 0.375).abs() < 0.001);
        // Two people at 200 % on top of 200 % stay within full scale, and louder stays louder.
        mixer.set_gain(9.0);
        mixer.clear();
        mixer.add(&[i16::MAX / 4; STEREO_FRAME_LEN], 2.0);
        let loud = mixer.finish()[0];
        mixer.clear();
        mixer.add(&[i16::MAX / 4; STEREO_FRAME_LEN], 2.0);
        mixer.add(&[i16::MAX / 4; STEREO_FRAME_LEN], 2.0);
        let louder = mixer.finish()[0];
        assert!(loud > 0.8 && louder > loud && louder <= 1.0, "{loud} {louder}");
        mixer.set_gain(0.0);
        mixer.clear();
        mixer.add(&[i16::MAX / 4; STEREO_FRAME_LEN], 1.0);
        assert_eq!(mixer.finish()[0], 0.0);
    }

    #[test]
    fn rms_of_silence_and_full_scale() {
        assert_eq!(rms(&[0; 64]), 0.0);
        assert!((rms(&[i16::MAX, -i16::MAX]) - 1.0).abs() < 0.001);
        assert_eq!(rms(&[]), 0.0);
    }
}
