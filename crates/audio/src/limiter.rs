//! A soft limiter for audio that may have been turned up past full scale.

/// Samples quieter than this are left as they are.
pub(crate) const KNEE: f32 = 0.8;

/// Bends samples above [`KNEE`] smoothly towards 1.0, so extra gain never wraps or hard-clips.
pub(crate) fn limit(frame: &mut [f32]) {
    const RANGE: f32 = 1.0 - KNEE;
    for sample in frame {
        let magnitude = sample.abs();
        if magnitude > KNEE {
            let bent = KNEE + RANGE * ((magnitude - KNEE) / RANGE).tanh();
            *sample = bent.copysign(*sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_quiet_samples_and_bends_loud_ones_below_full_scale() {
        let mut samples = [0.0, 0.5, -0.79, 0.9, -1.0, 2.0, -8.0];
        limit(&mut samples);
        assert_eq!(&samples[..3], &[0.0, 0.5, -0.79]);
        assert!(samples[3] > KNEE && samples[3] < 0.9);
        assert!(samples[4] < -KNEE && samples[4] > -1.0);
        assert!(samples[5] > samples[3] && samples[5] <= 1.0);
        assert!(samples[6] >= -1.0 && samples[6] < samples[4]);
    }
}
