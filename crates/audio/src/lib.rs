//! Audio capture and playback (WASAPI, through cpal), mixing, per-user volume, noise suppression and VAD.
//!
//! Everything runs at Discord's voice format: 48 kHz, 20 ms frames. Playback is always stereo.

mod devices;
mod mixer;
mod playback;

pub use devices::{AudioDevice, output_devices};
pub use mixer::{Mixer, rms};
pub use playback::{Playback, PlaybackSink};

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one 20 ms frame.
pub const FRAME_LEN: usize = 960;
/// Interleaved samples in one 20 ms stereo frame.
pub const STEREO_FRAME_LEN: usize = FRAME_LEN * 2;

pub type Error = cpal::Error;
