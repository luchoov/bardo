//! Send path: the microphone plays into songbird as a live raw track. songbird encodes and
//! encrypts it; the track is paused whenever the transmit gate closes, so songbird sends its five
//! silence frames and goes quiet, like the official client.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::Mutex,
};

use bardo_audio::FRAME_LEN;
use bardo_gateway::Id;
use rtrb::{Consumer, Producer, RingBuffer};
use songbird::{input::core::io::MediaSource, tracks::TrackHandle};
use tracing::warn;

use crate::{EventCallback, VoiceEvent};

/// Mono samples the microphone may queue: 200 ms.
const CAPACITY: usize = FRAME_LEN * 10;
/// Queued audio beyond this is stale (the track was paused or the mixer fell behind): skip it.
const MAX_QUEUED: usize = FRAME_LEN * 3;
const SAMPLE_BYTES: usize = size_of::<f32>();

/// Feeds microphone frames to the voice connection from the capture thread.
pub struct MicFeed {
    producer: Producer<f32>,
    track: TrackHandle,
    sending: bool,
    /// The track failed or ended; nothing more can be sent through it.
    dead: bool,
    /// Whether a full queue was already reported during this burst of speech.
    reported_full: bool,
    me: Id,
    on_event: EventCallback,
}

impl MicFeed {
    pub(crate) fn new(producer: Producer<f32>, track: TrackHandle, me: Id, on_event: EventCallback) -> Self {
        Self { producer, track, sending: false, dead: false, reported_full: false, me, on_event }
    }

    /// Queues a 20 ms mono frame if `open`, and starts or stops the track as the gate changes.
    pub fn push(&mut self, frame: &[f32], open: bool) {
        if self.dead {
            return;
        }
        if open {
            if self.producer.push_entire_slice(frame).is_err() && !self.reported_full {
                self.reported_full = true;
                warn!("microphone queue full: the voice connection is not reading it");
            }
            // Start after queueing, so the first read finds audio.
            if !self.sending {
                self.set_sending(true);
            }
        } else if self.sending {
            self.set_sending(false);
        }
    }

    fn set_sending(&mut self, sending: bool) {
        self.sending = sending;
        self.reported_full = false;
        let result = if sending { self.track.play() } else { self.track.pause() };
        if let Err(e) = result {
            warn!("the microphone track stopped working: {e}");
            self.dead = true;
            self.sending = false;
        }
        (self.on_event)(VoiceEvent::Transmitting { user_id: self.me, transmitting: self.sending });
    }
}

impl Drop for MicFeed {
    fn drop(&mut self) {
        if self.sending {
            self.set_sending(false);
        }
    }
}

pub(crate) fn channel() -> (Producer<f32>, MicSource) {
    let (producer, consumer) = RingBuffer::new(CAPACITY);
    (producer, MicSource { consumer: Mutex::new(consumer), partial: Vec::new() })
}

/// The track's byte stream: little-endian `f32` samples, read by songbird's mixer every 20 ms.
pub(crate) struct MicSource {
    consumer: Mutex<Consumer<f32>>,
    /// Bytes of a sample that did not fit in the previous read.
    partial: Vec<u8>,
}

impl Read for MicSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.partial.is_empty() {
            let len = self.partial.len().min(buf.len());
            buf[..len].copy_from_slice(&self.partial[..len]);
            self.partial.drain(..len);
            return Ok(len);
        }

        let mut consumer = self.consumer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let queued = consumer.slots();
        if queued > MAX_QUEUED
            && let Ok(stale) = consumer.read_chunk(queued - FRAME_LEN)
        {
            stale.commit_all();
        }

        // Never block or report the end of the stream: on an underrun, hand out one frame of
        // silence. A read of zero bytes would end the track.
        let wanted = (buf.len() / SAMPLE_BYTES).max(1);
        let available = consumer.slots().min(wanted);
        let mut written = 0;
        let mut write_sample = |sample: f32, buf: &mut [u8], partial: &mut Vec<u8>| {
            let bytes = sample.to_le_bytes();
            let fits = (buf.len() - written).min(SAMPLE_BYTES);
            buf[written..written + fits].copy_from_slice(&bytes[..fits]);
            partial.extend_from_slice(&bytes[fits..]);
            written += fits;
        };
        if available == 0 {
            for _ in 0..wanted.min(FRAME_LEN) {
                write_sample(0.0, buf, &mut self.partial);
            }
        } else if let Ok(chunk) = consumer.read_chunk(available) {
            let (first, second) = chunk.as_slices();
            for &sample in first.iter().chain(second) {
                write_sample(sample, buf, &mut self.partial);
            }
            chunk.commit_all();
        }
        Ok(written)
    }
}

impl Seek for MicSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for MicSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(bytes: &[u8]) -> Vec<f32> {
        bytes.as_chunks::<4>().0.iter().map(|&b| f32::from_le_bytes(b)).collect()
    }

    /// songbird disables symphonia's codecs by default; without the PCM decoder the microphone
    /// track fails with "no compatible track found".
    #[test]
    fn the_raw_track_codec_is_available() {
        use songbird::input::{codecs::get_codec_registry, core::codecs::CODEC_TYPE_PCM_F32LE};
        assert!(get_codec_registry().get_codec(CODEC_TYPE_PCM_F32LE).is_some());
    }

    #[test]
    fn reads_queued_samples_as_bytes() {
        let (mut producer, mut source) = channel();
        producer.push_entire_slice(&[0.25, -0.5]).unwrap();
        let mut buf = [0u8; 64];
        let read = source.read(&mut buf).unwrap();
        assert_eq!(samples(&buf[..read]), [0.25, -0.5]);
    }

    #[test]
    fn underrun_yields_silence_instead_of_ending() {
        let (_producer, mut source) = channel();
        let mut buf = vec![0xFFu8; FRAME_LEN * 8];
        let read = source.read(&mut buf).unwrap();
        assert_eq!(read, FRAME_LEN * 4);
        assert!(samples(&buf[..read]).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn skips_stale_audio() {
        let (mut producer, mut source) = channel();
        for frame in 0..5 {
            producer.push_entire_slice(&[frame as f32; FRAME_LEN]).unwrap();
        }
        let mut buf = vec![0u8; FRAME_LEN * 4];
        assert_eq!(source.read(&mut buf).unwrap(), buf.len());
        // Only the newest frame survives.
        assert!(samples(&buf).iter().all(|&s| s == 4.0));
    }

    #[test]
    fn splits_samples_across_short_reads() {
        let (mut producer, mut source) = channel();
        producer.push_entire_slice(&[1.5]).unwrap();
        let mut first = [0u8; 3];
        assert_eq!(source.read(&mut first).unwrap(), 3);
        let mut rest = [0u8; 8];
        assert_eq!(source.read(&mut rest).unwrap(), 1);
        let bytes = [first[0], first[1], first[2], rest[0]];
        assert_eq!(f32::from_le_bytes(bytes), 1.5);
    }
}
