//! Metering the microphone outside voice, so the voice activity threshold can be set before
//! joining a channel.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use bardo_audio::{Capture, TransmitControl, Transmitter};

/// Captures from the microphone and updates [`TransmitControl::level_db`] until dropped. Nothing
/// is sent anywhere.
pub struct MicTest {
    _capture: Capture,
    lost: Arc<AtomicBool>,
}

impl MicTest {
    pub fn start(device_id: Option<&str>, control: Arc<TransmitControl>) -> Result<Self, String> {
        let mut transmitter = Transmitter::new(control);
        let lost = Arc::new(AtomicBool::new(false));
        let flag = lost.clone();
        let capture = Capture::start(
            device_id,
            move |frame| {
                transmitter.process(frame);
            },
            move || flag.store(true, Ordering::Relaxed),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { _capture: capture, lost })
    }

    /// The microphone went away, or the default one changed: start a new test to keep metering.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }
}
