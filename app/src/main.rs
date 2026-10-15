#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(debug_assertions)]
mod demo;
mod hotkey;
mod settings;
mod token;
mod win;

use std::{cell::RefCell, collections::HashSet, rc::Rc, sync::Arc, time::Duration};

use bardo_core::{
    AudioDevice, AudioDevices, Client, ClientEvent, Command, ConnectionState, Id, MemberChange, SILENCE_DB,
    Snapshot, TransmitControl, UserAudio, VoiceStatus,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tracing::warn;
use tracing_subscriber::EnvFilter;

use crate::{
    hotkey::{Action, Binding, Hotkeys},
    settings::{HotkeySettings, Settings, VoiceSettings},
};

slint::include_modules!();

const SCREEN_LOGIN: i32 = 0;
const SCREEN_MAIN: i32 = 1;
/// How long an error notice stays up.
const NOTICE_DURATION: Duration = Duration::from_secs(8);
/// How often the input level meter refreshes while the settings are open.
const METER_INTERVAL: Duration = Duration::from_millis(50);

/// Which hotkey a key capture is for; the index is what the settings UI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyTarget {
    PushToTalk = 0,
    Mute = 1,
    Deafen = 2,
}

impl KeyTarget {
    fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::PushToTalk),
            1 => Some(Self::Mute),
            2 => Some(Self::Deafen),
            _ => None,
        }
    }
}

/// UI-thread state.
#[derive(Default)]
struct App {
    client: Option<Client>,
    /// Bumped on every connect and sign-out so events from a previous client are ignored.
    generation: u64,
    snapshot: Arc<Snapshot>,
    /// Users heard speaking in our voice channel.
    speaking: HashSet<Id>,
    settings: Settings,
    collapsed: HashSet<Id>,
    rows: Rc<VecModel<Row>>,
    /// Bumped whenever a notice is shown, so an older notice's timer does not clear a newer one.
    notice_generation: u64,
    transmit: Arc<TransmitControl>,
    hotkeys: Option<Hotkeys>,
    tray: Option<BardoTray>,
    /// Devices listed in the settings sheet, in its order (after "System default").
    inputs: Vec<AudioDevice>,
    outputs: Vec<AudioDevice>,
    capturing: Option<KeyTarget>,
    meter: slint::Timer,
}

thread_local! {
    static APP: RefCell<App> = RefCell::default();
}

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    token::init();
    // Dependencies enable both rustls crypto backends, so rustls cannot pick one by itself.
    if rustls::crypto::aws_lc_rs::default_provider().install_default().is_err() {
        warn!("a TLS crypto provider was already installed");
    }

    let start_in_tray = std::env::args().any(|arg| arg == win::TRAY_ARG);
    win::use_dark_menus();
    let window = MainWindow::new()?;
    let claimed = win::claim_single_instance({
        let weak = window.as_weak();
        move || {
            let _ = weak.upgrade_in_event_loop(|window| show_window(&window));
        }
    });
    if !claimed {
        return Ok(());
    }

    APP.with_borrow_mut(|app| {
        app.settings = Settings::load();
        apply_voice_settings(&app.transmit, &app.settings.voice);
        let hotkeys = Hotkeys::start(app.transmit.clone(), |action| {
            let _ = slint::invoke_from_event_loop(move || match action {
                Action::ToggleMute => toggle_mute(),
                Action::ToggleDeafen => toggle_deafen(),
            });
        });
        apply_hotkeys(&hotkeys, &app.settings);
        app.hotkeys = Some(hotkeys);
    });
    window.set_rows(ModelRc::from(APP.with_borrow(|app| app.rows.clone())));

    let tray = BardoTray::new()?;
    tray.on_show_window({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                show_window(&window);
            }
        }
    });
    tray.on_toggle_mute(toggle_mute);
    tray.on_toggle_deafen(toggle_deafen);
    tray.on_leave_voice(|| send(Command::LeaveVoice));
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    APP.with_borrow_mut(|app| app.tray = Some(tray));
    render_tray();

    window.window().on_close_requested(|| {
        if !APP.with_borrow(|app| app.settings.window.close_to_tray) {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    window.on_sign_in({
        let weak = window.as_weak();
        move |input| {
            let Some(window) = weak.upgrade() else { return };
            let Some(token) = token::normalize(&input) else {
                window.set_login_error("That doesn't look like a token.".into());
                return;
            };
            if let Err(e) = token::save(&token) {
                window.set_login_error(format!("Could not store the token: {e}").into());
                return;
            }
            connect(&window, token);
        }
    });

    window.on_sign_out({
        let weak = window.as_weak();
        move || {
            let Some(window) = weak.upgrade() else { return };
            token::clear();
            disconnect(&window, "");
        }
    });

    window.on_toggle_guild(|id| {
        let Some(id) = parse_id(&id) else { return };
        APP.with_borrow_mut(|app| {
            if !app.collapsed.remove(&id) {
                app.collapsed.insert(id);
            }
        });
        render();
    });

    window.on_dismiss_notice({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_notice("".into());
            }
        }
    });

    window.on_set_server_mute(|guild, user, on| modify_member(&guild, &user, MemberChange::ServerMute(on)));
    window.on_set_server_deafen(|guild, user, on| modify_member(&guild, &user, MemberChange::ServerDeafen(on)));
    window.on_move_member(|guild, user, channel| {
        if let Some(channel) = parse_id(&channel) {
            modify_member(&guild, &user, MemberChange::Move(Some(channel)));
        }
    });
    window.on_disconnect_member(|guild, user| modify_member(&guild, &user, MemberChange::Move(None)));

    window.on_set_local_mute(|user, muted| {
        update_user_audio(&user, |audio| audio.muted = muted);
    });

    window.on_edit_volume({
        let weak = window.as_weak();
        move |user, name| {
            let (Some(window), Some(user_id)) = (weak.upgrade(), parse_id(&user)) else { return };
            let volume = APP.with_borrow(|app| app.settings.user(user_id).volume);
            window.set_volume_value(f32::from(volume));
            window.set_volume_name(name);
            window.set_volume_user(user);
        }
    });

    window.on_set_volume(|user, volume| {
        let volume = u16::try_from(volume).unwrap_or_default();
        update_user_audio(&user, |audio| audio.volume = volume);
    });

    window.on_join_channel(|guild, channel| {
        if let (Some(guild_id), Some(channel_id)) = (parse_id(&guild), parse_id(&channel)) {
            send(Command::JoinVoice { guild_id, channel_id });
        }
    });
    window.on_leave_voice(|| send(Command::LeaveVoice));
    window.on_toggle_mute(toggle_mute);
    window.on_toggle_deafen(toggle_deafen);

    window.on_open_settings({
        let weak = window.as_weak();
        move || {
            let Some(window) = weak.upgrade() else { return };
            APP.with_borrow_mut(|app| {
                app.inputs = bardo_core::input_devices();
                app.outputs = bardo_core::output_devices();
                window.set_threshold(app.settings.voice.threshold_db);
                window.set_input_volume(f32::from(app.settings.voice.input_volume));
            });
            render_settings(&window);
            window.set_settings_open(true);
            let weak = window.as_weak();
            APP.with_borrow(|app| {
                app.meter.start(slint::TimerMode::Repeated, METER_INTERVAL, move || {
                    let Some(window) = weak.upgrade() else { return };
                    let level = APP.with_borrow(|app| {
                        if app.snapshot.voice.is_some() { app.transmit.level_db() } else { SILENCE_DB }
                    });
                    window.set_input_level(level);
                });
            });
        }
    });
    window.on_close_settings({
        let weak = window.as_weak();
        move || {
            APP.with_borrow(|app| app.meter.stop());
            if let Some(window) = weak.upgrade() {
                window.set_settings_open(false);
            }
        }
    });
    window.on_select_input(|index| {
        update_voice_settings(|voice, app| voice.input_device = device_at(&app.inputs, index));
        send_devices();
    });
    window.on_select_output(|index| {
        update_voice_settings(|voice, app| voice.output_device = device_at(&app.outputs, index));
        send_devices();
    });
    window.on_set_push_to_talk({
        let weak = window.as_weak();
        move |enabled| {
            update_voice_settings(|voice, _| voice.push_to_talk = enabled);
            if let Some(window) = weak.upgrade() {
                render_settings(&window);
            }
        }
    });
    window.on_set_threshold(|threshold| update_voice_settings(|voice, _| voice.threshold_db = threshold));
    window.on_set_noise_suppression(|enabled| update_voice_settings(|voice, _| voice.noise_suppression = enabled));
    window.on_set_input_volume(|volume| {
        let volume = volume.round().clamp(0.0, f32::from(settings::MAX_INPUT_VOLUME)) as u16;
        update_voice_settings(|voice, _| voice.input_volume = volume);
    });
    window.on_set_auto_gain(|enabled| update_voice_settings(|voice, _| voice.auto_gain = enabled));
    window.on_capture_key({
        let weak = window.as_weak();
        move |target| {
            let (Some(window), Some(target)) = (weak.upgrade(), KeyTarget::from_index(target)) else { return };
            APP.with_borrow_mut(|app| app.capturing = Some(target));
            render_settings(&window);
            let weak = window.as_weak();
            APP.with_borrow(|app| {
                let Some(hotkeys) = &app.hotkeys else { return };
                hotkeys.capture_next(move |binding| {
                    let _ = slint::invoke_from_event_loop(move || {
                        APP.with_borrow_mut(|app| app.capturing = None);
                        if binding.is_some() {
                            set_binding(target, binding);
                        }
                        if let Some(window) = weak.upgrade() {
                            render_settings(&window);
                        }
                    });
                });
            });
        }
    });
    window.on_clear_key({
        let weak = window.as_weak();
        move |target| {
            let Some(target) = KeyTarget::from_index(target) else { return };
            set_binding(target, None);
            if let Some(window) = weak.upgrade() {
                render_settings(&window);
            }
        }
    });
    window.on_set_autostart({
        let weak = window.as_weak();
        move |enabled| {
            win::set_autostart(enabled);
            if let Some(window) = weak.upgrade() {
                render_settings(&window);
            }
        }
    });
    window.on_set_close_to_tray(|enabled| {
        APP.with_borrow_mut(|app| {
            app.settings.window.close_to_tray = enabled;
            app.settings.save();
        });
    });

    window.on_close_volume({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_volume_user("".into());
            }
        }
    });

    #[cfg(debug_assertions)]
    if demo::enabled() {
        window.set_screen(SCREEN_MAIN);
        window.set_connected(true);
        window.set_status("Demo".into());
        APP.with_borrow_mut(|app| {
            app.snapshot = Arc::new(demo::snapshot());
            app.speaking = demo::speaking();
        });
        render();
        render_voice_bar(&window);
    } else {
        start(&window);
    }
    #[cfg(not(debug_assertions))]
    start(&window);

    if !start_in_tray {
        window.show()?;
    }
    // Keeps running while the window is hidden in the tray, until "Quit" (or closing the window
    // with close-to-tray off).
    slint::run_event_loop()?;
    window.hide()?;
    APP.with_borrow_mut(|app| app.tray = None);
    if let Some(client) = APP.with_borrow_mut(|app| app.client.take()) {
        client.shutdown();
    }
    Ok(())
}

fn start(window: &MainWindow) {
    match token::load() {
        Some(token) => connect(window, token),
        None => window.set_screen(SCREEN_LOGIN),
    }
}

fn show_window(window: &MainWindow) {
    if let Err(e) = window.show() {
        warn!("failed to show the window: {e}");
    }
    win::bring_to_front(window.window());
}

fn toggle_mute() {
    let (mute, deaf) = APP.with_borrow(|app| (app.snapshot.self_mute, app.snapshot.self_deaf));
    // Like the official client: unmuting while deafened also undeafens.
    if deaf {
        send(Command::SetSelfDeaf(false));
        send(Command::SetSelfMute(false));
    } else {
        send(Command::SetSelfMute(!mute));
    }
}

fn toggle_deafen() {
    let deaf = APP.with_borrow(|app| app.snapshot.self_deaf);
    send(Command::SetSelfDeaf(!deaf));
}

fn connect(window: &MainWindow, token: String) {
    window.set_login_error("".into());
    window.set_notice("".into());
    window.set_screen(SCREEN_MAIN);
    window.set_connected(false);
    window.set_status("Connecting…".into());

    let generation = APP.with_borrow_mut(|app| {
        app.client = None;
        app.generation += 1;
        app.generation
    });
    let weak = window.as_weak();
    let transmit = APP.with_borrow(|app| app.transmit.clone());
    let client = Client::start(token, transmit, move |event| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let current = APP.with_borrow(|app| app.generation);
            if let (Some(window), true) = (weak.upgrade(), current == generation) {
                on_client_event(&window, event);
            }
        });
    });
    APP.with_borrow_mut(|app| {
        for (&user_id, &audio) in &app.settings.users {
            client.send(Command::SetUserAudio { user_id, audio: user_audio(audio) });
        }
        client.send(Command::SetAudioDevices(audio_devices(&app.settings.voice)));
        if app.settings.voice.self_mute {
            client.send(Command::SetSelfMute(true));
        }
        if app.settings.voice.self_deaf {
            client.send(Command::SetSelfDeaf(true));
        }
        app.client = Some(client);
    });
}

fn disconnect(window: &MainWindow, error: &str) {
    APP.with_borrow_mut(|app| {
        app.client = None;
        app.generation += 1;
        app.snapshot = Arc::default();
        app.speaking.clear();
        app.collapsed.clear();
    });
    render();
    render_voice_bar(window);
    render_tray();
    window.set_volume_user("".into());
    window.set_login_error(error.into());
    window.set_screen(SCREEN_LOGIN);
}

fn on_client_event(window: &MainWindow, event: ClientEvent) {
    match event {
        ClientEvent::State(state) => {
            let status = match &state {
                ConnectionState::Connecting => "Connecting…".to_owned(),
                ConnectionState::Connected => "Connected".to_owned(),
                ConnectionState::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})…"),
                ConnectionState::Failed(reason) => format!("Disconnected: {reason}"),
                ConnectionState::AuthenticationFailed => {
                    token::clear();
                    disconnect(window, "Discord rejected the token. Sign in again.");
                    return;
                }
            };
            window.set_connected(state == ConnectionState::Connected);
            window.set_status(status.into());
            render_tray();
        }
        ClientEvent::Snapshot(snapshot) => {
            APP.with_borrow_mut(|app| {
                if snapshot.voice.is_none() {
                    app.speaking.clear();
                }
                let voice = &mut app.settings.voice;
                if (voice.self_mute, voice.self_deaf) != (snapshot.self_mute, snapshot.self_deaf) {
                    (voice.self_mute, voice.self_deaf) = (snapshot.self_mute, snapshot.self_deaf);
                    app.settings.save();
                }
                app.snapshot = snapshot;
            });
            render();
            render_voice_bar(window);
            render_tray();
        }
        ClientEvent::Speaking(users) => {
            APP.with_borrow_mut(|app| app.speaking = users.into_iter().collect());
            update_speaking();
        }
        ClientEvent::TokenRefreshed(token) => {
            if let Err(e) = token::save(&token) {
                warn!("failed to store refreshed token: {e}");
            }
        }
        ClientEvent::Notice(message) => show_notice(window, message),
    }
}

fn show_notice(window: &MainWindow, message: String) {
    window.set_notice(message.into());
    let generation = APP.with_borrow_mut(|app| {
        app.notice_generation += 1;
        app.notice_generation
    });
    let weak = window.as_weak();
    slint::Timer::single_shot(NOTICE_DURATION, move || {
        let current = APP.with_borrow(|app| app.notice_generation);
        if let (Some(window), true) = (weak.upgrade(), current == generation) {
            window.set_notice("".into());
        }
    });
}

fn send(command: Command) {
    APP.with_borrow(|app| {
        if let Some(client) = &app.client {
            client.send(command);
        }
    });
}

fn modify_member(guild: &str, user: &str, change: MemberChange) {
    if let (Some(guild_id), Some(user_id)) = (parse_id(guild), parse_id(user)) {
        send(Command::ModifyMember { guild_id, user_id, change });
    }
}

fn update_user_audio(user: &str, update: impl FnOnce(&mut settings::UserAudio)) {
    let Some(user_id) = parse_id(user) else { return };
    let audio = APP.with_borrow_mut(|app| {
        app.settings.update_user(user_id, update);
        app.settings.save();
        app.settings.user(user_id)
    });
    send(Command::SetUserAudio { user_id, audio: user_audio(audio) });
    render();
}

/// Changes a voice setting, saves it and applies it to the microphone pipeline.
fn update_voice_settings(update: impl FnOnce(&mut VoiceSettings, &App)) {
    APP.with_borrow_mut(|app| {
        let mut voice = app.settings.voice.clone();
        update(&mut voice, app);
        if voice == app.settings.voice {
            return;
        }
        apply_voice_settings(&app.transmit, &voice);
        if let Some(hotkeys) = &app.hotkeys {
            hotkeys.set_push_to_talk(voice.push_to_talk_key);
        }
        app.settings.voice = voice;
        app.settings.save();
    });
}

fn update_hotkeys(update: impl FnOnce(&mut HotkeySettings)) {
    APP.with_borrow_mut(|app| {
        update(&mut app.settings.hotkeys);
        if let Some(hotkeys) = &app.hotkeys {
            apply_hotkeys(hotkeys, &app.settings);
        }
        app.settings.save();
    });
}

fn apply_hotkeys(hotkeys: &Hotkeys, settings: &Settings) {
    hotkeys.set_push_to_talk(settings.voice.push_to_talk_key);
    hotkeys.set_binding(Action::ToggleMute, settings.hotkeys.toggle_mute);
    hotkeys.set_binding(Action::ToggleDeafen, settings.hotkeys.toggle_deafen);
}

fn set_binding(target: KeyTarget, binding: Option<Binding>) {
    match target {
        // Push-to-talk is a single key; modifiers would only get in the way mid-game.
        KeyTarget::PushToTalk => update_voice_settings(|voice, _| voice.push_to_talk_key = binding.map(|b| b.key)),
        KeyTarget::Mute => update_hotkeys(|hotkeys| hotkeys.toggle_mute = binding),
        KeyTarget::Deafen => update_hotkeys(|hotkeys| hotkeys.toggle_deafen = binding),
    }
}

fn apply_voice_settings(transmit: &TransmitControl, voice: &VoiceSettings) {
    transmit.set_push_to_talk(voice.push_to_talk);
    transmit.set_threshold_db(voice.threshold_db);
    transmit.set_noise_suppression(voice.noise_suppression);
    transmit.set_input_gain(voice.input_gain());
    transmit.set_auto_gain(voice.auto_gain);
}

fn audio_devices(voice: &VoiceSettings) -> AudioDevices {
    AudioDevices { input: voice.input_device.clone(), output: voice.output_device.clone() }
}

fn send_devices() {
    let devices = APP.with_borrow(|app| audio_devices(&app.settings.voice));
    send(Command::SetAudioDevices(devices));
}

/// The device at a settings combo box index; index 0 is "System default".
fn device_at(devices: &[AudioDevice], index: i32) -> Option<String> {
    let index = usize::try_from(index).ok()?.checked_sub(1)?;
    devices.get(index).map(|device| device.id.clone())
}

fn render_settings(window: &MainWindow) {
    let view = APP.with_borrow(|app| {
        let voice = &app.settings.voice;
        let names = |devices: &[AudioDevice]| {
            let names: Vec<slint::SharedString> = std::iter::once("System default".into())
                .chain(devices.iter().map(|device| device.name.as_str().into()))
                .collect();
            ModelRc::new(VecModel::from(names))
        };
        let index = |devices: &[AudioDevice], id: &Option<String>| {
            id.as_ref()
                .and_then(|id| devices.iter().position(|device| &device.id == id))
                .map_or(0, |position| position as i32 + 1)
        };
        VoiceSettingsView {
            inputs: names(&app.inputs),
            outputs: names(&app.outputs),
            input_index: index(&app.inputs, &voice.input_device),
            output_index: index(&app.outputs, &voice.output_device),
            push_to_talk: voice.push_to_talk,
            key_name: voice.push_to_talk_key.map(hotkey::key_name).unwrap_or_default().into(),
            mute_key: app.settings.hotkeys.toggle_mute.map(Binding::name).unwrap_or_default().into(),
            deafen_key: app.settings.hotkeys.toggle_deafen.map(Binding::name).unwrap_or_default().into(),
            capturing: app.capturing.map_or(-1, |target| target as i32),
            autostart: win::autostart_enabled(),
            close_to_tray: app.settings.window.close_to_tray,
            noise_suppression: voice.noise_suppression,
            auto_gain: voice.auto_gain,
        }
    });
    window.set_voice_settings(view);
}

fn user_audio(audio: settings::UserAudio) -> UserAudio {
    UserAudio { volume: audio.volume, muted: audio.muted }
}

fn parse_id(id: &str) -> Option<Id> {
    id.parse().ok()
}

/// Flattens the snapshot into list rows, honoring collapsed guilds.
fn render() {
    APP.with_borrow(|app| {
        let mut rows = Vec::new();
        for guild in &app.snapshot.guilds {
            let guild_id: slint::SharedString = guild.id.to_string().into();
            let expanded = !app.collapsed.contains(&guild.id);
            rows.push(Row {
                kind: RowKind::Guild,
                id: guild_id.clone(),
                label: guild.name.as_str().into(),
                expanded,
                ..Default::default()
            });
            if !expanded {
                continue;
            }
            for channel in &guild.channels {
                let current = app.snapshot.voice.as_ref().is_some_and(|voice| voice.channel_id == channel.id);
                rows.push(Row {
                    kind: RowKind::Channel,
                    id: channel.id.to_string().into(),
                    label: channel.name.as_str().into(),
                    guild_id: guild_id.clone(),
                    count: channel.participants.len() as i32,
                    limit: channel.user_limit as i32,
                    can_connect: channel.can_connect,
                    current,
                    ..Default::default()
                });
                if channel.participants.is_empty() {
                    continue;
                }

                // Every participant of a channel can be moved to the same channels.
                let targets: Vec<Target> = guild
                    .channels
                    .iter()
                    .filter(|target| target.can_connect && target.id != channel.id)
                    .map(|target| Target { id: target.id.to_string().into(), name: target.name.as_str().into() })
                    .collect();
                let targets = ModelRc::new(VecModel::from(targets));

                for participant in &channel.participants {
                    let audio = app.settings.user(participant.user_id);
                    rows.push(Row {
                        kind: RowKind::Participant,
                        id: participant.user_id.to_string().into(),
                        label: participant.name.as_str().into(),
                        guild_id: guild_id.clone(),
                        initial: initial(&participant.name).into(),
                        self_mute: participant.self_mute,
                        self_deaf: participant.self_deaf,
                        server_mute: participant.server_mute,
                        server_deaf: participant.server_deaf,
                        live: participant.streaming,
                        speaking: current && app.speaking.contains(&participant.user_id),
                        is_me: participant.is_me,
                        local_mute: audio.muted && !participant.is_me,
                        volume: i32::from(audio.volume),
                        can_mute: participant.moderation.mute,
                        can_deafen: participant.moderation.deafen,
                        can_move: participant.moderation.move_,
                        targets: targets.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        app.rows.set_vec(rows);
    });
}

/// Refreshes only the speaking flags, which change several times a second.
fn update_speaking() {
    APP.with_borrow(|app| {
        for index in 0..app.rows.row_count() {
            let Some(mut row) = app.rows.row_data(index) else { continue };
            if row.kind != RowKind::Participant {
                continue;
            }
            let speaking = parse_id(&row.id).is_some_and(|id| app.speaking.contains(&id));
            if row.speaking != speaking {
                row.speaking = speaking;
                app.rows.set_row_data(index, row);
            }
        }
    });
}

fn render_voice_bar(window: &MainWindow) {
    let bar = APP.with_borrow(|app| match &app.snapshot.voice {
        Some(voice) => VoiceBar {
            active: true,
            connected: voice.status == VoiceStatus::Connected,
            status: match voice.status {
                VoiceStatus::Connected => "Voice connected",
                VoiceStatus::Connecting | VoiceStatus::Disconnected => "Connecting…",
            }
            .into(),
            channel: voice.channel_name.as_str().into(),
            guild: voice.guild_name.as_str().into(),
            muted: voice.self_mute,
            deafened: voice.self_deaf,
        },
        None => VoiceBar::default(),
    });
    window.set_voice(bar);
}

fn render_tray() {
    APP.with_borrow(|app| {
        let Some(tray) = &app.tray else { return };
        let snapshot = &app.snapshot;
        tray.set_connected(app.client.is_some());
        tray.set_muted(snapshot.self_mute || snapshot.self_deaf);
        tray.set_deafened(snapshot.self_deaf);
        tray.set_in_voice(snapshot.voice.is_some());
        let mut tooltip = String::from("bardo");
        if let Some(voice) = &snapshot.voice {
            tooltip.push_str(" — ");
            tooltip.push_str(&voice.channel_name);
        }
        if snapshot.self_deaf {
            tooltip.push_str(" (deafened)");
        } else if snapshot.self_mute {
            tooltip.push_str(" (muted)");
        }
        tray.set_status_text(tooltip.into());
    });
}

fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}
