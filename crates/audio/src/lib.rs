//! Audio capture and playback (WASAPI, through cpal), mixing, per-user volume, noise suppression and VAD.
//!
//! Everything runs at Discord's voice format: 48 kHz, 20 ms frames. Playback is always stereo.

mod capture;
mod codec;
mod devices;
mod limiter;
mod mixer;
mod playback;
mod resample;
mod sounds;
mod transmit;

pub use capture::Capture;
pub use codec::{VOICE_BITRATE, VoiceCodec};
pub use devices::{AudioDevice, input_devices, output_devices};
pub use mixer::{Mixer, rms, rms_f32};
pub use playback::{Playback, PlaybackSink};
pub use sounds::{Ringer, Sound, play as play_sound, play_paced};
pub use transmit::{DEFAULT_THRESHOLD_DB, SILENCE_DB, TransmitControl, Transmitter, level_db};

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one 20 ms frame.
pub const FRAME_LEN: usize = 960;
/// Interleaved samples in one 20 ms stereo frame.
pub const STEREO_FRAME_LEN: usize = FRAME_LEN * 2;

pub type Error = cpal::Error;
