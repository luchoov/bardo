use cpal::traits::{DeviceTrait, HostTrait};
use tracing::warn;

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
