//! Settings that stay on this device, stored as JSON in `%APPDATA%\bardo\settings.json`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::PathBuf,
};

use bardo_core::{DEFAULT_THRESHOLD_DB, Id};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::hotkey::Binding;

pub const DEFAULT_VOLUME: u16 = 100;
pub const MAX_VOLUME: u16 = 200;
/// Input volume range in percent, like the official client's.
pub const MAX_INPUT_VOLUME: u16 = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub voice: VoiceSettings,
    /// Per-user playback settings, keyed by user id. Users with default settings are left out.
    #[serde(default)]
    pub users: BTreeMap<Id, UserAudio>,
    #[serde(default)]
    pub hotkeys: HotkeySettings,
    #[serde(default)]
    pub window: WindowSettings,
    #[serde(default)]
    pub servers: ServerSettings,
    /// Changes are kept in memory only (the demo).
    #[serde(skip)]
    pub ephemeral: bool,
}

/// Which servers are listed in full, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    /// In the order they were pinned.
    pub pinned: Vec<Id>,
    /// Servers whose empty voice channels are listed too.
    pub show_empty: BTreeSet<Id>,
}

impl ServerSettings {
    pub fn is_pinned(&self, guild_id: Id) -> bool {
        self.pinned.contains(&guild_id)
    }

    pub fn set_pinned(&mut self, guild_id: Id, pinned: bool) {
        self.pinned.retain(|&id| id != guild_id);
        if pinned {
            self.pinned.push(guild_id);
        }
    }

    pub fn toggle_show_empty(&mut self, guild_id: Id) {
        if !self.show_empty.remove(&guild_id) {
            self.show_empty.insert(guild_id);
        }
    }
}

/// Global hotkeys besides push-to-talk, which lives in [`VoiceSettings`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    pub toggle_mute: Option<Binding>,
    pub toggle_deafen: Option<Binding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    /// The close button hides bardo in the tray instead of quitting.
    pub close_to_tray: bool,
    /// Logical size, as last left.
    pub size: Option<[f32; 2]>,
    /// Physical position of the top-left corner, as last left.
    pub position: Option<[i32; 2]>,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self { close_to_tray: true, size: None, position: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Device ids; `None` follows the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub push_to_talk: bool,
    /// Windows virtual-key code.
    pub push_to_talk_key: Option<u32>,
    /// A click when the push-to-talk key goes down and up (with [`Self::sounds`] on).
    pub push_to_talk_sounds: bool,
    /// Voice activity threshold in dBFS.
    pub threshold_db: f32,
    pub noise_suppression: bool,
    /// Microphone volume in percent, 0–200.
    pub input_volume: u16,
    pub auto_gain: bool,
    /// Our mute and deafen, kept across restarts like in the official client.
    pub self_mute: bool,
    pub self_deaf: bool,
    /// Short sounds for mute and deafen, people joining and leaving, and calls.
    pub sounds: bool,
}

impl VoiceSettings {
    /// The linear gain for [`Self::input_volume`].
    pub fn input_gain(&self) -> f32 {
        f32::from(self.input_volume.min(MAX_INPUT_VOLUME)) / 100.0
    }
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            push_to_talk: false,
            push_to_talk_key: None,
            push_to_talk_sounds: false,
            threshold_db: DEFAULT_THRESHOLD_DB,
            noise_suppression: true,
            input_volume: DEFAULT_VOLUME,
            auto_gain: true,
            self_mute: false,
            self_deaf: false,
            sounds: true,
        }
    }
}

/// How a user sounds to us. Applies in every server and call, like in the official client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAudio {
    /// Playback volume in percent, 0–200.
    pub volume: u16,
    pub muted: bool,
}

impl Default for UserAudio {
    fn default() -> Self {
        Self { volume: DEFAULT_VOLUME, muted: false }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = path() else { return Self::default() };
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!("ignoring unreadable settings file {}: {e}", path.display());
                Self::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!("failed to read settings: {e}");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if self.ephemeral {
            return;
        }
        let Some(path) = path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| fs::write(&path, serde_json::to_vec_pretty(self).unwrap_or_default()));
        if let Err(e) = result {
            warn!("failed to save settings: {e}");
        }
    }

    pub fn user(&self, user_id: Id) -> UserAudio {
        self.users.get(&user_id).copied().unwrap_or_default()
    }

    pub fn update_user(&mut self, user_id: Id, update: impl FnOnce(&mut UserAudio)) {
        let mut audio = self.user(user_id);
        update(&mut audio);
        audio.volume = audio.volume.min(MAX_VOLUME);
        if audio == UserAudio::default() {
            self.users.remove(&user_id);
        } else {
            self.users.insert(user_id, audio);
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("bardo").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_users_are_not_stored() {
        let mut settings = Settings::default();
        settings.update_user(Id(1), |audio| audio.volume = 150);
        settings.update_user(Id(2), |audio| audio.muted = true);
        assert_eq!(settings.user(Id(1)), UserAudio { volume: 150, muted: false });

        settings.update_user(Id(1), |audio| audio.volume = 100);
        assert!(!settings.users.contains_key(&Id(1)));

        let json = serde_json::to_string(&settings.users).unwrap();
        assert_eq!(json, r#"{"2":{"volume":100,"muted":true}}"#);
        let json = serde_json::to_string(&settings).unwrap();
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user(Id(2)), UserAudio { volume: 100, muted: true });
    }

    #[test]
    fn older_files_get_default_voice_settings() {
        let settings: Settings = serde_json::from_str(r#"{"users": {}}"#).unwrap();
        assert_eq!(settings.voice, VoiceSettings::default());
        let settings: Settings = serde_json::from_str(r#"{"voice": {"push_to_talk": true}}"#).unwrap();
        assert!(settings.voice.push_to_talk);
        assert!(settings.voice.noise_suppression);
        assert!(settings.voice.auto_gain);
        assert_eq!(settings.voice.input_gain(), 1.0);
        assert!(settings.window.close_to_tray);
        assert_eq!(settings.hotkeys, HotkeySettings::default());
    }

    #[test]
    fn pinning_keeps_the_order_and_no_duplicates() {
        let mut servers = ServerSettings::default();
        servers.set_pinned(Id(2), true);
        servers.set_pinned(Id(1), true);
        servers.set_pinned(Id(2), true);
        assert_eq!(servers.pinned, [Id(1), Id(2)]);
        servers.set_pinned(Id(1), false);
        assert_eq!(servers.pinned, [Id(2)]);
        assert!(servers.is_pinned(Id(2)));
    }

    #[test]
    fn volume_is_clamped() {
        let mut settings = Settings::default();
        settings.update_user(Id(1), |audio| audio.volume = 500);
        assert_eq!(settings.user(Id(1)).volume, MAX_VOLUME);
    }
}
