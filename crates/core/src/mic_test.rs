//! Metering the microphone outside voice, so the voice activity threshold can be set before
//! joining a channel.

use std::sync::Arc;

use bardo_audio::{Capture, TransmitControl, Transmitter};

/// Captures from the microphone and updates [`TransmitControl::level_db`] until dropped. Nothing
/// is sent anywhere.
pub struct MicTest {
    _capture: Capture,
}

impl MicTest {
    pub fn start(device_id: Option<&str>, control: Arc<TransmitControl>) -> Result<Self, String> {
        let mut transmitter = Transmitter::new(control);
        let capture = Capture::start(device_id, move |frame| {
            transmitter.process(frame);
        })
        .map_err(|e| e.to_string())?;
        Ok(Self { _capture: capture })
    }
}
