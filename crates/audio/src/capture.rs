use cpal::{
    FromSample, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use tracing::{info, warn};

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

/// Turns device-format audio into 20 ms mono frames at 48 kHz.
struct Framer {
    channels: usize,
    resampler: Resampler,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    frame: Vec<f32>,
}

impl Framer {
    fn new(rate: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            resampler: Resampler::new(rate, SAMPLE_RATE),
            mono: Vec::new(),
            resampled: Vec::new(),
            frame: Vec::with_capacity(FRAME_LEN),
        }
    }

    fn push(&mut self, samples: impl Iterator<Item = f32>, on_frame: &mut impl FnMut(&mut [f32])) {
        self.mono.clear();
        let scale = 1.0 / self.channels as f32;
        let mut sum = 0.0;
        for (index, sample) in samples.enumerate() {
            sum += sample;
            if (index + 1) % self.channels == 0 {
                self.mono.push(sum * scale);
                sum = 0.0;
            }
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
}
