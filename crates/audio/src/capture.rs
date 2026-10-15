use cpal::{
    FromSample, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use tracing::{debug, info, warn};

use crate::{Error, FRAME_LEN, SAMPLE_RATE, devices, resample::Resampler};

/// An open input stream. Capture stops when it is dropped.
pub struct Capture {
    _stream: cpal::Stream,
}

impl Capture {
    /// Opens the input device with `device_id` (or the default one) in its native format and calls
    /// `on_frame`, on the audio thread, with 20 ms mono frames at 48 kHz in [-1, 1].
    pub fn start(
        device_id: Option<&str>,
        on_frame: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Result<Self, Error> {
        let device = devices::input_device(device_id)
            .ok_or_else(|| Error::with_message(cpal::ErrorKind::DeviceNotAvailable, "no input device"))?;
        let supported = device.default_input_config()?;
        let config = supported.config();
        if let Some(described) = devices::describe(&device) {
            info!(
                device = %described.name,
                rate = config.sample_rate,
                channels = config.channels,
                format = %supported.sample_format(),
                "opening input device"
            );
        }

        let framer = Framer::new(config.sample_rate, usize::from(config.channels));
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config, framer, on_frame),
            SampleFormat::I16 => build::<i16>(&device, config, framer, on_frame),
            SampleFormat::I32 => build::<i32>(&device, config, framer, on_frame),
            SampleFormat::U16 => build::<u16>(&device, config, framer, on_frame),
            other => Err(Error::with_message(
                cpal::ErrorKind::UnsupportedConfig,
                format!("unsupported input sample format {other}"),
            )),
        }?;
        stream.play()?;
        Ok(Self { _stream: stream })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut framer: Framer,
    mut on_frame: impl FnMut(&mut [f32]) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| framer.push(data.iter().map(|&sample| sample.to_sample::<f32>()), &mut on_frame),
        |e| warn!("capture stream error: {e}"),
        None,
    )
}

/// A channel this much quieter than the loudest one (power ratio, -20 dB) is left out of the
/// downmix: some headsets put the microphone on one channel and leave the other silent, and
/// averaging would halve the voice.
const QUIET_CHANNEL: f32 = 0.01;
/// Smoothing for the per-channel power estimate, per device callback.
const POWER_SMOOTHING: f32 = 0.1;

/// Turns device-format audio into 20 ms mono frames at 48 kHz.
struct Framer {
    channels: usize,
    resampler: Resampler,
    /// Smoothed power per channel, to find the channels that carry the voice.
    power: Vec<f32>,
    active: Vec<bool>,
    interleaved: Vec<f32>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    frame: Vec<f32>,
}

impl Framer {
    fn new(rate: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            resampler: Resampler::new(rate, SAMPLE_RATE),
            power: vec![0.0; channels.max(1)],
            active: vec![true; channels.max(1)],
            interleaved: Vec::new(),
            mono: Vec::new(),
            resampled: Vec::new(),
            frame: Vec::with_capacity(FRAME_LEN),
        }
    }

    fn push(&mut self, samples: impl Iterator<Item = f32>, on_frame: &mut impl FnMut(&mut [f32])) {
        self.interleaved.clear();
        self.interleaved.extend(samples);
        self.update_active_channels();

        self.mono.clear();
        let scale = 1.0 / self.active.iter().filter(|&&active| active).count().max(1) as f32;
        for frame in self.interleaved.chunks_exact(self.channels) {
            let sum: f32 = frame.iter().zip(&self.active).filter(|&(_, &active)| active).map(|(&s, _)| s).sum();
            self.mono.push(sum * scale);
        }

        self.resampled.clear();
        self.resampler.process(&self.mono, &mut self.resampled);
        for &sample in &self.resampled {
            self.frame.push(sample);
            if self.frame.len() == FRAME_LEN {
                on_frame(&mut self.frame);
                self.frame.clear();
            }
        }
    }

    fn update_active_channels(&mut self) {
        if self.channels == 1 {
            return;
        }
        let frames = (self.interleaved.len() / self.channels).max(1) as f32;
        for (channel, power) in self.power.iter_mut().enumerate() {
            let block: f32 =
                self.interleaved.iter().skip(channel).step_by(self.channels).map(|s| s * s).sum::<f32>() / frames;
            *power += (block - *power) * POWER_SMOOTHING;
        }
        let loudest = self.power.iter().copied().fold(0.0, f32::max);
        let mut changed = false;
        for (active, &power) in self.active.iter_mut().zip(&self.power) {
            let now = loudest <= f32::EPSILON || power >= loudest * QUIET_CHANNEL;
            changed |= now != *active;
            *active = now;
        }
        if changed {
            debug!(active = ?self.active, "microphone channels in use changed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_and_frames() {
        let mut framer = Framer::new(SAMPLE_RATE, 2);
        let mut frames: Vec<Vec<f32>> = Vec::new();

        // 15 ms of stereo audio: not a whole frame yet.
        let stereo: Vec<f32> = [0.5, 0.1].repeat(720);
        framer.push(stereo.iter().copied(), &mut |frame: &mut [f32]| frames.push(frame.to_vec()));
        assert!(frames.is_empty());
        framer.push(stereo.iter().copied(), &mut |frame: &mut [f32]| frames.push(frame.to_vec()));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), FRAME_LEN);
        assert!((frames[0][0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn a_silent_channel_does_not_halve_the_voice() {
        let mut framer = Framer::new(SAMPLE_RATE, 2);
        let mut frames: Vec<Vec<f32>> = Vec::new();
        let left_only: Vec<f32> = [0.5, 0.0].repeat(FRAME_LEN);
        framer.push(left_only.iter().copied(), &mut |frame: &mut [f32]| frames.push(frame.to_vec()));
        assert_eq!(frames.len(), 1);
        assert!((frames[0][0] - 0.5).abs() < 1e-6);
    }
}
