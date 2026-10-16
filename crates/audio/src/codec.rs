//! Opus the way the voice connection sends it, so the microphone test can play back what the
//! others hear.

use opus2::{Application, Bitrate, Channels, Decoder, Encoder};

use crate::{FRAME_LEN, SAMPLE_RATE, STEREO_FRAME_LEN};

/// What songbird's encoder is set to: stereo, music mode, 128 kbps.
pub const VOICE_BITRATE: i32 = 128_000;
/// Opus never needs more than this for a 20 ms packet.
const MAX_PACKET: usize = 1275;

pub struct VoiceCodec {
    encoder: Encoder,
    decoder: Decoder,
    stereo: Vec<f32>,
    packet: Vec<u8>,
}

impl VoiceCodec {
    pub fn new() -> Result<Self, String> {
        let mut encoder =
            Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio).map_err(|e| e.to_string())?;
        encoder.set_bitrate(Bitrate::Bits(VOICE_BITRATE)).map_err(|e| e.to_string())?;
        let decoder = Decoder::new(SAMPLE_RATE, Channels::Stereo).map_err(|e| e.to_string())?;
        Ok(Self { encoder, decoder, stereo: vec![0.0; STEREO_FRAME_LEN], packet: vec![0; MAX_PACKET] })
    }

    /// Encodes a 20 ms mono frame like the voice connection does (on both channels) and decodes it
    /// again, into `out` as interleaved stereo.
    pub fn round_trip(&mut self, frame: &[f32], out: &mut [f32]) -> Result<(), String> {
        debug_assert_eq!(frame.len(), FRAME_LEN);
        for (pair, &sample) in self.stereo.as_chunks_mut::<2>().0.iter_mut().zip(frame) {
            pair.fill(sample);
        }
        let len = self.encoder.encode_float(&self.stereo, &mut self.packet).map_err(|e| e.to_string())?;
        self.decoder.decode_float(&self.packet[..len], out, false).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level_db;

    #[test]
    fn round_trip_keeps_the_level() {
        let mut codec = VoiceCodec::new().unwrap();
        let mut out = vec![0.0; STEREO_FRAME_LEN];
        let mut phase = 0;
        let mut levels = Vec::new();
        for _ in 0..20 {
            let frame: Vec<f32> = (phase..phase + FRAME_LEN)
                .map(|i| 0.1 * (std::f32::consts::TAU * 300.0 * i as f32 / SAMPLE_RATE as f32).sin())
                .collect();
            phase += FRAME_LEN;
            codec.round_trip(&frame, &mut out).unwrap();
            levels.push(level_db(&out));
        }
        // Past the codec's start-up delay, a -23 dBFS tone comes back at the same level.
        let expected = 20.0 * (0.1 / 2f32.sqrt()).log10();
        assert!((levels[10] - expected).abs() < 1.0, "{} vs {expected}", levels[10]);
        assert!(out.as_chunks::<2>().0.iter().all(|[left, right]| (left - right).abs() < 0.02));
    }
}
