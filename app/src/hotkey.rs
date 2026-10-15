//! The push-to-talk key, polled with `GetAsyncKeyState`. Unlike `RegisterHotKey` this does not
//! take the key away from the game, and unlike a low-level hook it cannot delay anyone's input.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};

use bardo_core::TransmitControl;
use tracing::warn;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const VK_LBUTTON: u32 = 0x01;
const VK_RBUTTON: u32 = 0x02;
const VK_ESCAPE: u32 = 0x1B;
/// Shift, Ctrl and Alt without a side; the left/right variants are reported too and are preferred.
const GENERIC_MODIFIERS: std::ops::RangeInclusive<u32> = 0x10..=0x12;

type CaptureCallback = Box<dyn FnOnce(Option<u32>) + Send>;

pub struct PushToTalk {
    shared: Arc<Shared>,
}

struct Shared {
    /// Virtual-key code, 0 for none.
    key: AtomicU32,
    capture: Mutex<Option<CaptureCallback>>,
    stop: AtomicBool,
}

impl PushToTalk {
    pub fn start(control: Arc<TransmitControl>, key: Option<u32>) -> Self {
        let shared = Arc::new(Shared {
            key: AtomicU32::new(key.unwrap_or(0)),
            capture: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let poller = shared.clone();
        if let Err(e) = thread::Builder::new().name("bardo-ptt".into()).spawn(move || run(&poller, &control)) {
            warn!("failed to start the push-to-talk poller: {e}");
        }
        Self { shared }
    }

    pub fn set_key(&self, key: Option<u32>) {
        self.shared.key.store(key.unwrap_or(0), Ordering::Relaxed);
    }

    /// Reports the next key or mouse button pressed, or `None` if Escape cancels.
    pub fn capture_next(&self, on_key: impl FnOnce(Option<u32>) + Send + 'static) {
        *self.shared.capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(on_key));
    }
}

impl Drop for PushToTalk {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn run(shared: &Shared, control: &TransmitControl) {
    // Keys already down when a capture starts (the click on "Set key", say) only count once released.
    let mut ignored = [false; 256];
    let mut capturing = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let mut capture = shared.capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if capture.is_some() {
            if !capturing {
                capturing = true;
                for (vk, ignored) in ignored.iter_mut().enumerate() {
                    *ignored = is_down(vk as u32);
                }
            }
            if let Some(vk) = newly_pressed(&mut ignored)
                && let Some(on_key) = capture.take()
            {
                capturing = false;
                drop(capture);
                on_key((vk != VK_ESCAPE).then_some(vk));
            }
        } else {
            capturing = false;
            drop(capture);
            let key = shared.key.load(Ordering::Relaxed);
            control.set_ptt_pressed(key != 0 && is_down(key));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn newly_pressed(ignored: &mut [bool; 256]) -> Option<u32> {
    for vk in 1..=254u32 {
        let down = is_down(vk);
        let slot = &mut ignored[vk as usize];
        if !down {
            *slot = false;
            continue;
        }
        if *slot || matches!(vk, VK_LBUTTON | VK_RBUTTON) || GENERIC_MODIFIERS.contains(&vk) {
            continue;
        }
        return Some(vk);
    }
    None
}

fn is_down(vk: u32) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

/// A readable name for a virtual-key code, e.g. "Caps Lock" or "Mouse 4".
pub fn key_name(vk: u32) -> String {
    match vk {
        0x04 => return "Mouse 3".into(),
        0x05 => return "Mouse 4".into(),
        0x06 => return "Mouse 5".into(),
        _ => {}
    }
    // SAFETY: plain value conversions and a write into a buffer of the stated length.
    let name = unsafe {
        let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX);
        let extended = if scan & 0xFF00 == 0xE000 { 1 << 24 } else { 0 };
        let lparam = ((scan & 0xFF) << 16 | extended) as i32;
        let mut buffer = [0u16; 64];
        let len = GetKeyNameTextW(lparam, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    };
    if name.is_empty() { format!("Key {vk:#04X}") } else { name }
}
