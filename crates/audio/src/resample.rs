/// Streaming linear-interpolation resampler for mono audio. Plenty for voice, which Opus
/// band-limits anyway; microphones at 48 kHz skip it entirely.
pub(crate) struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output sample, where 0 is `previous` and 1 is the first new input.
    position: f64,
    /// The last input sample of the previous call.
    previous: f32,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self { step: f64::from(from_rate) / f64::from(to_rate), position: 0.0, previous: 0.0 }
    }

    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.step == 1.0 {
            output.extend_from_slice(input);
            return;
        }
        let Some(&last) = input.last() else { return };
        let len = input.len();
        let at = |index: usize| if index == 0 { self.previous } else { input[index - 1] };
        while self.position < len as f64 {
            let index = self.position as usize;
            let fraction = (self.position - index as f64) as f32;
            let (a, b) = (at(index), at(index + 1));
            output.push(a + (b - a) * fraction);
            self.position += self.step;
        }
        self.position -= len as f64;
        self.previous = last;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_passes_through() {
        let mut resampler = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        resampler.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, [0.1, 0.2]);
    }

    #[test]
    fn produces_the_right_number_of_samples_across_calls() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            resampler.process(&[0.0; 441], &mut out);
        }
        // One second of 44.1 kHz in, one second of 48 kHz out (give or take one sample).
        assert!((out.len() as i64 - 48_000).abs() <= 1, "{}", out.len());
    }

    #[test]
    fn interpolates_between_samples() {
        let mut resampler = Resampler::new(24_000, 48_000);
        let mut out = Vec::new();
        resampler.process(&[1.0, 1.0], &mut out);
        // Starts from the previous (silent) sample and ramps up halfway between points.
        assert_eq!(out, [0.0, 0.5, 1.0, 1.0]);
    }
}
