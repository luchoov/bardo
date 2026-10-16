use cpal::{
    ErrorKind,
    traits::{DeviceTrait, HostTrait},
};
use tracing::{debug, info, warn};

use crate::Error;

/// An audio endpoint as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// Stable identifier, suitable for storing in settings.
    pub id: String,
    pub name: String,
}

pub fn output_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.filter_map(|device| describe(&device)).collect(),
        Err(e) => {
            warn!("failed to list output devices: {e}");
            Vec::new()
        }
    }
}

pub fn input_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.filter_map(|device| describe(&device)).collect(),
        Err(e) => {
            warn!("failed to list input devices: {e}");
            Vec::new()
        }
    }
}

pub(crate) fn describe(device: &cpal::Device) -> Option<AudioDevice> {
    let id = device.id().ok()?.to_string();
    let name = device.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| id.clone());
    Some(AudioDevice { id, name })
}

/// The output device with `id`, or the system default when it is `None` or no longer present.
pub(crate) fn output_device(id: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    let chosen = id.and_then(|id| {
        host.output_devices()
            .ok()?
            .find(|device| device.id().is_ok_and(|device_id| device_id.to_string() == id))
    });
    chosen.or_else(|| host.default_output_device())
}

/// The input device with `id`, or the system default when it is `None` or no longer present.
pub(crate) fn input_device(id: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    let chosen = id.and_then(|id| {
        host.input_devices()
            .ok()?
            .find(|device| device.id().is_ok_and(|device_id| device_id.to_string() == id))
    });
    chosen.or_else(|| host.default_input_device())
}

/// The error callback for a stream. Calls `on_lost`, once, when the stream stops working for good:
/// its device went away, or the system default it follows changed (WASAPI never moves a stream to
/// the new default by itself). Anything else is only logged.
pub(crate) fn error_handler(
    stream: &'static str,
    on_lost: impl FnOnce() + Send + 'static,
) -> impl FnMut(Error) + Send + 'static {
    let mut on_lost = Some(on_lost);
    move |e| match e.kind() {
        ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => {
            if let Some(on_lost) = on_lost.take() {
                info!("{stream} stream lost: {e}");
                on_lost();
            }
        }
        ErrorKind::Xrun => debug!("{stream} stream glitch: {e}"),
        _ => warn!("{stream} stream error: {e}"),
    }
}
