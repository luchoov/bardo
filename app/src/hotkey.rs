//! Global hotkeys (push-to-talk, toggle mute, toggle deafen), polled with `GetAsyncKeyState`.
//! Unlike `RegisterHotKey` this does not take the keys away from the game, and unlike a low-level
//! hook it cannot delay anyone's input.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};

use bardo_core::TransmitControl;
use serde::{Deserialize, Serialize};
use tracing::warn;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const VK_LBUTTON: u32 = 0x01;
const VK_RBUTTON: u32 = 0x02;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
/// Shift, Ctrl and Alt without a side; the left/right variants are reported too and are preferred.
const GENERIC_MODIFIERS: std::ops::RangeInclusive<u32> = VK_SHIFT..=VK_MENU;
/// Left/right Shift, Ctrl and Alt.
const SIDED_MODIFIERS: std::ops::RangeInclusive<u32> = 0xA0..=0xA5;

pub const CTRL: u8 = 1;
pub const SHIFT: u8 = 2;
pub const ALT: u8 = 4;
pub const WIN: u8 = 8;

/// A key or mouse button, plus the modifiers that must be held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Windows virtual-key code.
    pub key: u32,
    #[serde(default)]
    pub modifiers: u8,
}

impl Binding {
    fn pack(binding: Option<Self>) -> u32 {
        binding.map_or(0, |b| (b.key & 0xFF) | u32::from(b.modifiers) << 8)
    }

    fn unpack(packed: u32) -> Option<Self> {
        let key = packed & 0xFF;
        (key != 0).then_some(Self { key, modifiers: (packed >> 8) as u8 })
    }

    /// Whether the key and all the modifiers are down. Extra modifiers do not matter, so a
    /// hotkey still works while, say, Shift is held to walk.
    fn is_down(self) -> bool {
        is_down(self.key) && self.modifiers & !held_modifiers() == 0
    }

    /// The keys to press, modifiers first, e.g. ["Ctrl", "Shift", "M"] or ["Mouse 4"].
    pub fn parts(self) -> Vec<String> {
        let mut parts: Vec<String> = [(CTRL, "Ctrl"), (SHIFT, "Shift"), (ALT, "Alt"), (WIN, "Win")]
            .into_iter()
            .filter(|&(bit, _)| self.modifiers & bit != 0)
            .map(|(_, name)| name.to_owned())
            .collect();
        parts.push(key_name(self.key));
        parts
    }

    /// A readable name, e.g. "Ctrl + Shift + M".
    #[cfg(test)]
    fn name(self) -> String {
        self.parts().join(" + ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
}

const ACTIONS: [Action; 2] = [Action::ToggleMute, Action::ToggleDeafen];

type CaptureCallback = Box<dyn FnOnce(Option<Binding>) + Send>;
type ActionCallback = Box<dyn Fn(Action) + Send>;

pub struct Hotkeys {
    shared: Arc<Shared>,
}

struct Shared {
    /// The push-to-talk virtual-key code, 0 for none. Modifiers are ignored for push-to-talk.
    push_to_talk: AtomicU32,
    /// Packed [`Binding`]s, in [`ACTIONS`] order.
    actions: [AtomicU32; ACTIONS.len()],
    capture: Mutex<Option<CaptureCallback>>,
    stop: AtomicBool,
}

impl Hotkeys {
    /// Starts polling. `on_action` runs on the polling thread.
    pub fn start(control: Arc<TransmitControl>, on_action: impl Fn(Action) + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            push_to_talk: AtomicU32::new(0),
            actions: Default::default(),
            capture: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let poller = shared.clone();
        let on_action: ActionCallback = Box::new(on_action);
        let spawned = thread::Builder::new().name("bardo-hotkeys".into()).spawn(move || run(&poller, &control, &on_action));
        if let Err(e) = spawned {
            warn!("failed to start the hotkey poller: {e}");
        }
        Self { shared }
    }

    pub fn set_push_to_talk(&self, key: Option<u32>) {
        self.shared.push_to_talk.store(key.unwrap_or(0) & 0xFF, Ordering::Relaxed);
    }

    pub fn set_binding(&self, action: Action, binding: Option<Binding>) {
        let index = ACTIONS.iter().position(|&a| a == action).unwrap_or_default();
        self.shared.actions[index].store(Binding::pack(binding), Ordering::Relaxed);
    }

    /// Reports the next key, mouse button or combination pressed, or `None` if Escape cancels.
    /// A modifier on its own counts once it is released without another key.
    pub fn capture_next(&self, on_binding: impl FnOnce(Option<Binding>) + Send + 'static) {
        *self.shared.capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(on_binding));
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn run(shared: &Shared, control: &TransmitControl, on_action: &ActionCallback) {
    let mut capture: Option<Capture> = None;
    // Whether each action's binding was down on the last poll: actions fire on the press only.
    let mut was_down = [false; ACTIONS.len()];
    while !shared.stop.load(Ordering::Relaxed) {
        let mut pending = shared.capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.is_some() {
            let state = capture.get_or_insert_with(Capture::start);
            if let Some(result) = state.poll()
                && let Some(on_binding) = pending.take()
            {
                capture = None;
                drop(pending);
                on_binding(result);
                // The keys just captured are still down; they must not fire their new action.
                was_down = [true; ACTIONS.len()];
            }
        } else {
            capture = None;
            drop(pending);
            let key = shared.push_to_talk.load(Ordering::Relaxed);
            control.set_ptt_pressed(key != 0 && is_down(key));
            for ((&action, packed), was_down) in ACTIONS.iter().zip(&shared.actions).zip(&mut was_down) {
                let down = Binding::unpack(packed.load(Ordering::Relaxed)).is_some_and(Binding::is_down);
                if down && !*was_down {
                    on_action(action);
                }
                *was_down = down;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Key capture in progress.
struct Capture {
    /// Keys already down when the capture started (the click on "Set key", say) only count once
    /// released; keys already reported are ignored until released too.
    ignored: [bool; 256],
    /// A modifier pressed on its own; it becomes the binding if released before any other key.
    modifier: Option<u32>,
}

impl Capture {
    fn start() -> Self {
        let mut ignored = [false; 256];
        for (vk, ignored) in ignored.iter_mut().enumerate() {
            *ignored = is_down(vk as u32);
        }
        Self { ignored, modifier: None }
    }

    /// `Some(result)` once the capture is over.
    fn poll(&mut self) -> Option<Option<Binding>> {
        for vk in 1..=254u32 {
            let slot = &mut self.ignored[vk as usize];
            if !is_down(vk) {
                *slot = false;
                if self.modifier == Some(vk) {
                    return Some(Some(Binding { key: vk, modifiers: 0 }));
                }
                continue;
            }
            if *slot || matches!(vk, VK_LBUTTON | VK_RBUTTON) || GENERIC_MODIFIERS.contains(&vk) {
                continue;
            }
            *slot = true;
            if vk == VK_ESCAPE {
                return Some(None);
            }
            if is_modifier(vk) {
                self.modifier.get_or_insert(vk);
                continue;
            }
            return Some(Some(Binding { key: vk, modifiers: held_modifiers() }));
        }
        None
    }
}

fn is_modifier(vk: u32) -> bool {
    SIDED_MODIFIERS.contains(&vk) || matches!(vk, VK_LWIN | VK_RWIN)
}

fn held_modifiers() -> u8 {
    let mut modifiers = 0;
    if is_down(VK_CONTROL) {
        modifiers |= CTRL;
    }
    if is_down(VK_SHIFT) {
        modifiers |= SHIFT;
    }
    if is_down(VK_MENU) {
        modifiers |= ALT;
    }
    if is_down(VK_LWIN) || is_down(VK_RWIN) {
        modifiers |= WIN;
    }
    modifiers
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
        VK_LWIN => return "Left Win".into(),
        VK_RWIN => return "Right Win".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_pack_and_unpack() {
        let binding = Binding { key: 0x4D, modifiers: CTRL | SHIFT };
        assert_eq!(Binding::unpack(Binding::pack(Some(binding))), Some(binding));
        assert_eq!(Binding::unpack(Binding::pack(None)), None);
    }

    #[test]
    fn names_list_modifiers_first() {
        assert_eq!(Binding { key: 0x05, modifiers: CTRL | ALT }.name(), "Ctrl + Alt + Mouse 4");
    }
}
