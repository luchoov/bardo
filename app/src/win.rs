//! Windows integration: dark native menus, starting with Windows, a single running instance and
//! bringing the window to the front.

use std::{ffi::c_void, iter, path::Path, ptr, thread};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tracing::warn;
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, HWND, POINT},
    Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW},
        Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject},
    },
    UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx, IsIconic,
        SW_RESTORE, SetForegroundWindow, ShowWindow,
    },
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "bardo";
/// Passed by the autostart entry: start hidden in the tray.
pub const TRAY_ARG: &str = "--tray";
/// Signalled by a second instance to ask the running one to show itself.
const SHOW_EVENT: &str = "Local\\bardo-show-window";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

/// Makes the native menus (the tray menu) dark. Uses uxtheme's `SetPreferredAppMode`, which is
/// exported by ordinal only; it has been stable since Windows 10 1903 and is what other apps use.
pub fn use_dark_menus() {
    const SET_PREFERRED_APP_MODE: usize = 135;
    const FLUSH_MENU_THEMES: usize = 136;
    const FORCE_DARK: i32 = 2;
    // SAFETY: the ordinals have these signatures on every Windows version that exports them, and
    // a missing export is checked for.
    unsafe {
        let uxtheme = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if uxtheme.is_null() {
            return;
        }
        if let Some(set_mode) = GetProcAddress(uxtheme, SET_PREFERRED_APP_MODE as *const u8) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            set_mode(FORCE_DARK);
        }
        if let Some(flush) = GetProcAddress(uxtheme, FLUSH_MENU_THEMES as *const u8) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

/// Whether bardo starts with Windows.
pub fn autostart_enabled() -> bool {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut size = 0u32;
    // SAFETY: a size query with valid strings and no output buffer.
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, ptr::null_mut(), ptr::null_mut(), &mut size)
    };
    status == 0
}

/// Adds or removes the entry that starts bardo, hidden in the tray, when signing in to Windows.
pub fn set_autostart(enabled: bool) {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    let status = if enabled {
        let Ok(exe) = std::env::current_exe() else { return };
        let command: Vec<u16> = command_line(&exe).encode_utf16().chain(iter::once(0)).collect();
        // SAFETY: valid strings; the data length is in bytes and includes the terminator.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                command.as_ptr().cast::<c_void>(),
                (command.len() * 2) as u32,
            )
        }
    } else {
        // SAFETY: valid strings.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => 0,
            status => status,
        }
    };
    if status != 0 {
        warn!(status, enabled, "failed to change the autostart entry");
    }
}

fn command_line(exe: &Path) -> String {
    format!("\"{}\" {TRAY_ARG}", exe.display())
}

/// Claims the single instance. Returns `false` if bardo is already running, after asking that
/// instance to show its window. Otherwise `on_show` runs (on a background thread) whenever a
/// later instance asks.
pub fn claim_single_instance(on_show: impl Fn() + Send + 'static) -> bool {
    let name = wide(SHOW_EVENT);
    // SAFETY: an auto-reset, unsignalled named event; the handle lives for the whole process.
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, name.as_ptr()) };
    if event.is_null() {
        // Without the event we cannot tell; run anyway.
        return true;
    }
    // SAFETY: reads the calling thread's last error, set by CreateEventW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: plain calls on a valid event handle. We were started by the user, so we may
        // pass on the right to take the foreground.
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
            SetEvent(event);
        }
        return false;
    }
    let event = SendHandle(event);
    let spawned = thread::Builder::new().name("bardo-instance".into()).spawn(move || {
        let event = event;
        // SAFETY: waits on a handle that is never closed.
        while unsafe { WaitForSingleObject(event.0, INFINITE) } == 0 {
            on_show();
        }
    });
    if let Err(e) = spawned {
        warn!("failed to watch for other instances: {e}");
    }
    true
}

struct SendHandle(HANDLE);
// SAFETY: event handles may be used from any thread.
unsafe impl Send for SendHandle {}

/// Whether a window placed at `x, y` would have its title bar on a connected monitor.
pub fn on_screen(x: i32, y: i32) -> bool {
    let point = POINT { x: x + 60, y: y + 12 };
    // SAFETY: a plain query.
    !unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) }.is_null()
}

fn hwnd(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    let handle = handle.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return None };
    Some(handle.hwnd.get() as HWND)
}

/// Flashes the taskbar button until the window comes to the front, without taking the focus
/// (a game keeps it).
pub fn flash(window: &slint::Window) {
    let Some(hwnd) = hwnd(window) else { return };
    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: a valid window and a fully initialized structure.
    unsafe { FlashWindowEx(&info) };
}

/// Restores the window if minimized and brings it to the front.
pub fn bring_to_front(window: &slint::Window) {
    let Some(hwnd) = hwnd(window) else { return };
    // SAFETY: a valid top-level window of this process.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_quotes_the_path() {
        let line = command_line(Path::new(r"C:\Program Files\bardo\bardo.exe"));
        assert_eq!(line, r#""C:\Program Files\bardo\bardo.exe" --tray"#);
    }
}
