use cpal::{
    BufferSize, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use rtrb::{Consumer, Producer, RingBuffer};
use tracing::{info, warn};

use crate::{Error, SAMPLE_RATE, STEREO_FRAME_LEN, devices};

/// 500 ms of stereo audio.
const CAPACITY: usize = SAMPLE_RATE as usize;
/// Audio queued before playback starts, and again after an underrun, to absorb network jitter.
const PREBUFFER: usize = STEREO_FRAME_LEN * 2;
/// Above this much queued audio, the excess is dropped so latency stays bounded when the
/// device consumes slower than the network delivers.
const MAX_QUEUED: usize = STEREO_FRAME_LEN * 6;

/// An open output stream. Playback stops when it is dropped.
pub struct Playback {
    _stream: cpal::Stream,
}

/// Feeds audio to a [`Playback`] from another thread.
pub struct PlaybackSink {
    producer: Producer<f32>,
}

impl Playback {
    /// Opens the output device with `device_id` (or the default one) at 48 kHz stereo.
    /// WASAPI converts to the device's own format. `on_lost` is called once if the device goes
    /// away, or the default changes while playing on it; the playback must then be started again.
    pub fn start(
        device_id: Option<&str>,
        on_lost: impl FnOnce() + Send + 'static,
    ) -> Result<(Self, PlaybackSink), Error> {
        let device = devices::output_device(device_id)
            .ok_or_else(|| Error::with_message(cpal::ErrorKind::DeviceNotAvailable, "no output device"))?;
        if let Some(described) = devices::describe(&device) {
            info!(device = %described.name, "opening output device");
        }

        let config = StreamConfig { channels: 2, sample_rate: SAMPLE_RATE, buffer_size: BufferSize::Default };
        let (producer, mut consumer) = RingBuffer::new(CAPACITY);
        let mut primed = false;
        let stream = device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| fill(&mut consumer, &mut primed, data),
            devices::error_handler("playback", on_lost),
            None,
        )?;
        stream.play()?;
        Ok((Self { _stream: stream }, PlaybackSink { producer }))
    }
}

impl PlaybackSink {
    /// Queues interleaved stereo samples. Drops them if the device has stopped consuming.
    pub fn push(&mut self, samples: &[f32]) {
        if self.producer.push_entire_slice(samples).is_err() {
            warn!("playback buffer full, dropping audio");
        }
    }
}

/// The device callback: plays queued audio. `data` arrives filled with silence.
fn fill(consumer: &mut Consumer<f32>, primed: &mut bool, data: &mut [f32]) {
    let queued = consumer.slots();
    if !*primed {
        if queued < PREBUFFER {
            return;
        }
        *primed = true;
    }
    if queued > MAX_QUEUED {
        // Keep whole stereo frames so channels stay aligned.
        let excess = (queued - PREBUFFER) & !1;
        if let Ok(chunk) = consumer.read_chunk(excess) {
            chunk.commit_all();
        }
    }
    let (_, missing) = consumer.pop_partial_slice(data);
    if !missing.is_empty() {
        *primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(producer: &mut Producer<f32>, frames: usize) {
        for _ in 0..frames {
            producer.push_entire_slice(&[0.5; STEREO_FRAME_LEN]).unwrap();
        }
    }

    #[test]
    fn waits_for_the_prebuffer_then_plays() {
        let (mut producer, mut consumer) = RingBuffer::new(CAPACITY);
        let mut primed = false;
        let mut out = [0.0; 480];

        queue(&mut producer, 1);
        fill(&mut consumer, &mut primed, &mut out);
        assert!(!primed);
        assert_eq!(out[0], 0.0);

        queue(&mut producer, 1);
        fill(&mut consumer, &mut primed, &mut out);
        assert!(primed);
        assert_eq!(out[0], 0.5);
    }

    #[test]
    fn underrun_rebuffers() {
        let (mut producer, mut consumer) = RingBuffer::new(CAPACITY);
        let mut primed = true;
        queue(&mut producer, 1);
        let mut out = vec![0.0; STEREO_FRAME_LEN * 2];
        fill(&mut consumer, &mut primed, &mut out);
        assert!(!primed);
        assert_eq!(out[STEREO_FRAME_LEN - 1], 0.5);
        assert_eq!(out[STEREO_FRAME_LEN], 0.0);
    }

    #[test]
    fn drops_excess_latency() {
        let (mut producer, mut consumer) = RingBuffer::new(CAPACITY);
        let mut primed = true;
        queue(&mut producer, 10);
        let mut out = [0.0; 2];
        fill(&mut consumer, &mut primed, &mut out);
        assert_eq!(consumer.slots(), PREBUFFER - 2);
    }
}
