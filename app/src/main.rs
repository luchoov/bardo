#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cue;
#[cfg(debug_assertions)]
mod demo;
mod hotkey;
mod logging;
mod pictures;
mod qr;
mod settings;
mod token;
mod update;
mod view;
mod win;

use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use bardo_core::{
    AudioDevice, AudioDevices, Client, ClientEvent, Command, ConnectionState, Id, MemberChange, MicTest,
    MicTestKind, MicTestPhase, Package, QrLogin, QrLoginEvent, Release, Ringer, Snapshot, Sound, TransmitControl,
    UpdateEvent, UserAudio, VoiceStatus,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tracing::{debug, warn};

use crate::{
    cue::SoundGate,
    hotkey::{Action, Binding, Hotkeys},
    pictures::Pictures,
    settings::{HotkeySettings, Settings, VoiceSettings},
    view::{Context, ListState},
};

slint::include_modules!();

const SCREEN_LOGIN: i32 = 0;
const SCREEN_MAIN: i32 = 1;
/// The sign-in QR code: waiting for one, showing it, scanned, or not available.
const QR_WAITING: i32 = 0;
const QR_SHOWN: i32 = 1;
const QR_SCANNED: i32 = 2;
const QR_UNAVAILABLE: i32 = 3;
/// The new-version bar: offering it, downloading it, or failed to.
const UPDATE_OFFERED: i32 = 0;
const UPDATE_DOWNLOADING: i32 = 1;
const UPDATE_FAILED: i32 = 2;
/// How long a freshly updated bardo waits for the old one to exit.
const UPDATE_HANDOVER: Duration = Duration::from_secs(10);
/// How long an error notice stays up.
const NOTICE_DURATION: Duration = Duration::from_secs(8);
/// How often the input level meter refreshes while the settings are open.
const METER_INTERVAL: Duration = Duration::from_millis(50);
/// How long the ringback plays while nobody answers.
const RINGBACK_LIMIT: Duration = Duration::from_secs(60);
/// Linked from the sign-in screen.
const TOKEN_GUIDE: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/docs/token.md");

/// Which hotkey a key capture is for; the index is what the settings page uses.
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
    window: Option<slint::Weak<MainWindow>>,
    client: Option<Client>,
    /// Bumped on every connect and sign-out so events from a previous client are ignored.
    generation: u64,
    connection: Option<ConnectionState>,
    snapshot: Arc<Snapshot>,
    /// Users heard speaking in our voice channel, and us while transmitting.
    speaking: HashSet<Id>,
    settings: Settings,
    list: ListState,
    /// The guilds whose members the client fetches, as last sent.
    watched: HashSet<Id>,
    rows: Rc<VecModel<Row>>,
    /// Bumped whenever a notice is shown, so an older notice's timer does not clear a newer one.
    notice_generation: u64,
    transmit: Arc<TransmitControl>,
    hotkeys: Option<Hotkeys>,
    tray: Option<BardoTray>,
    /// Devices listed in the settings, in their order (after "System default").
    inputs: Vec<AudioDevice>,
    outputs: Vec<AudioDevice>,
    capturing: Option<KeyTarget>,
    meter: slint::Timer,
    /// Rings while a call is calling us.
    ringer: Option<Ringer>,
    /// The call that is ringing us, so a new one flashes the taskbar again.
    ringing: Option<Id>,
    /// Plays while a call we are in rings the others.
    ringback: Option<Ringback>,
    sound_gate: SoundGate,
    /// Meters the microphone while the settings are open outside voice, or lets us hear it.
    mic_test: Option<MicTest>,
    /// The test that should be running.
    mic_test_kind: MicTestKind,
    /// A newer version, offered for download.
    update: Option<Release>,
    /// A downloaded update, started once bardo has closed.
    pending_update: Option<update::Pending>,
    /// Signing in with a QR code, while the sign-in screen is up.
    qr_login: Option<QrLogin>,
    /// Bumped whenever QR sign-in starts or stops, so events from an older one are ignored.
    qr_generation: u64,
    /// Profile pictures and server icons fetched so far.
    pictures: Pictures,
}

struct Ringback {
    call: Id,
    since: Instant,
    ringer: Option<Ringer>,
}

thread_local! {
    static APP: RefCell<App> = RefCell::default();
}

fn main() -> Result<(), slint::PlatformError> {
    logging::init();
    token::init();
    // reqwest only uses a process-wide rustls crypto provider; songbird and the gateway's websocket
    // already bring ring.
    if rustls::crypto::ring::default_provider().install_default().is_err() {
        warn!("a TLS crypto provider was already installed");
    }

    let start_in_tray = std::env::args().any(|arg| arg == win::TRAY_ARG);
    // Started by the bardo it replaces, which is still closing.
    if let Some(pid) = update::replaced_process() {
        win::wait_for_exit(pid, UPDATE_HANDOVER);
    }
    win::use_dark_menus();
    let window = MainWindow::new()?;
    // The demo may run next to a real bardo, e.g. to take screenshots.
    #[cfg(debug_assertions)]
    let single = !demo::enabled();
    #[cfg(not(debug_assertions))]
    let single = true;
    let claimed = !single
        || win::claim_single_instance({
            let weak = window.as_weak();
            move || {
                let _ = weak.upgrade_in_event_loop(|window| show_window(&window));
            }
        });
    if !claimed {
        return Ok(());
    }
    update::clean_up();

    APP.with_borrow_mut(|app| {
        app.window = Some(window.as_weak());
        app.settings = Settings::load();
        apply_voice_settings(&app.transmit, &app.settings.voice);
        app.transmit.set_auto_gain_db(app.settings.voice.learned_gain());
        let hotkeys = Hotkeys::start(app.transmit.clone(), |action| {
            let _ = slint::invoke_from_event_loop(move || match action {
                Action::ToggleMute => toggle_mute(),
                Action::ToggleDeafen => toggle_deafen(),
                Action::PushToTalk(pressed) => push_to_talk_sound(pressed),
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

    restore_geometry(&window);
    window.window().on_close_requested({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                save_geometry(&window);
            }
            if !APP.with_borrow(|app| app.settings.window.close_to_tray) {
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    wire_main(&window);
    wire_list(&window);
    wire_settings(&window);

    #[cfg(debug_assertions)]
    let _sound_demo = demo::sounds().then(run_sound_demo);
    #[cfg(debug_assertions)]
    if demo::enabled() {
        window.set_screen(if demo::login() { SCREEN_LOGIN } else { SCREEN_MAIN });
        APP.with_borrow_mut(|app| {
            app.connection = Some(ConnectionState::Connected);
            app.snapshot = Arc::new(demo::snapshot());
            app.speaking = demo::speaking();
            app.settings.ephemeral = true;
            app.settings.servers.pinned = demo::pinned();
        });
        if demo::login() {
            start_qr_login(&window);
        }
        if demo::update() {
            window.set_update_version("0.2.0".into());
        }
        update_ringing(&window);
        render_all();
        if demo::settings() {
            window.invoke_open_settings();
        }
        if let Some(query) = demo::search() {
            window.set_search(query.into());
            window.invoke_search_edited(query.into());
        }
        if demo::tour() {
            run_tour(Rc::new(RefCell::new(demo::tour_script().into_iter())));
        }
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
    if window.window().is_visible() {
        save_geometry(&window);
    }
    window.hide()?;
    APP.with_borrow_mut(|app| {
        app.tray = None;
        remember_gain(app);
    });
    if let Some(client) = APP.with_borrow_mut(|app| app.client.take()) {
        client.shutdown();
    }
    if let Some(pending) = APP.with_borrow_mut(|app| app.pending_update.take()) {
        update::launch(pending);
        // The installer replaces the exe, so leave at once rather than tearing down the rest.
        std::process::exit(0);
    }
    Ok(())
}

fn restore_geometry(window: &MainWindow) {
    let (size, position) = APP.with_borrow(|app| (app.settings.window.size, app.settings.window.position));
    if let Some([width, height]) = size {
        window.window().set_size(slint::LogicalSize::new(width, height));
    }
    if let Some([x, y]) = position.filter(|&[x, y]| win::on_screen(x, y)) {
        window.window().set_position(slint::PhysicalPosition::new(x, y));
    }
}

fn save_geometry(window: &MainWindow) {
    let window = window.window();
    if window.is_minimized() || window.is_maximized() {
        return;
    }
    let size = window.size().to_logical(window.scale_factor());
    let position = window.position();
    update_settings(|settings| {
        settings.window.size = Some([size.width, size.height]);
        settings.window.position = Some([position.x, position.y]);
    });
}

fn start(window: &MainWindow) {
    match token::load() {
        Some(token) => connect(window, token),
        None => {
            window.set_screen(SCREEN_LOGIN);
            start_qr_login(window);
        }
    }
}

fn show_window(window: &MainWindow) {
    if let Err(e) = window.show() {
        warn!("failed to show the window: {e}");
    }
    win::bring_to_front(window.window());
}

/// Sign-in, the header, the footer and notices.
fn wire_main(window: &MainWindow) {
    window.on_sign_in({
        let weak = window.as_weak();
        move |input| {
            if let Some(window) = weak.upgrade() {
                sign_in(&window, &input);
            }
        }
    });

    window.on_open_token_guide(|| win::open(TOKEN_GUIDE));
    window.on_install_update({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                install_update(&window);
            }
        }
    });
    window.on_open_update(open_release_page);

    window.on_dismiss_notice({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_notice("".into());
            }
        }
    });

    window.on_toggle_mute(toggle_mute);
    window.on_toggle_deafen(toggle_deafen);

    window.on_search_edited(|query| {
        APP.with_borrow_mut(|app| app.list.query = query.to_string());
        render();
    });
    window.on_search_accepted(|| {
        let first = APP.with_borrow(|app| view::first_result(&app.rows.iter().collect::<Vec<_>>()));
        match first {
            Some((guild, channel)) if guild.is_empty() => start_call(&channel),
            Some((guild, channel)) => join(&guild, &channel),
            None => {}
        }
    });
    window.on_accept_call(|channel| join("", &channel));
    window.on_decline_call(|channel| {
        if let Some(channel_id) = parse_id(&channel) {
            send(Command::DeclineCall { channel_id });
        }
        // Stop ringing at once; Discord confirms with CALL_UPDATE.
        APP.with_borrow_mut(|app| app.ringer = None);
    });
}

/// The list's rows and menus.
fn wire_list(window: &MainWindow) {
    let api = window.global::<Api>();
    api.on_toggle_section(|key| {
        APP.with_borrow_mut(|app| match key.as_str() {
            view::SECTION_FRIENDS => app.list.friends_collapsed = !app.list.friends_collapsed,
            view::SECTION_OTHER => app.list.other_open = Some(!app.list.other_open(&app.settings)),
            _ => {}
        });
        render();
    });
    api.on_toggle_guild(|guild| {
        let Some(guild_id) = parse_id(&guild) else { return };
        APP.with_borrow_mut(|app| {
            let in_main = app.settings.servers.is_pinned(guild_id)
                || app.snapshot.voice.as_ref().is_some_and(|voice| voice.guild_id == Some(guild_id));
            let set = if in_main { &mut app.list.collapsed } else { &mut app.list.expanded };
            if !set.remove(&guild_id) {
                set.insert(guild_id);
            }
        });
        render();
    });
    api.on_toggle_empty(|guild| {
        let Some(guild_id) = parse_id(&guild) else { return };
        update_settings(|settings| settings.servers.toggle_show_empty(guild_id));
        render();
    });
    api.on_pin_guild(|guild, pinned| {
        let Some(guild_id) = parse_id(&guild) else { return };
        APP.with_borrow_mut(|app| {
            // A newly pinned server shows up expanded.
            app.list.collapsed.remove(&guild_id);
            app.list.expanded.remove(&guild_id);
        });
        update_settings(|settings| settings.servers.set_pinned(guild_id, pinned));
        render();
    });
    api.on_join_channel(|guild, channel| join(&guild, &channel));
    api.on_start_call(|channel| start_call(&channel));
    api.on_leave_voice(|| send(Command::LeaveVoice));
    api.on_set_server_mute(|guild, user, on| modify_member(&guild, &user, MemberChange::ServerMute(on)));
    api.on_set_server_deafen(|guild, user, on| modify_member(&guild, &user, MemberChange::ServerDeafen(on)));
    api.on_move_member(|guild, user, channel| {
        if let Some(channel) = parse_id(&channel) {
            modify_member(&guild, &user, MemberChange::Move(Some(channel)));
        }
    });
    api.on_disconnect_member(|guild, user| modify_member(&guild, &user, MemberChange::Move(None)));
    api.on_set_local_mute(|user, muted| update_user_audio(&user, |audio| audio.muted = muted));
    api.on_set_volume(|user, volume| {
        let volume = u16::try_from(volume).unwrap_or_default();
        update_user_audio(&user, |audio| audio.volume = volume);
    });
}

fn wire_settings(window: &MainWindow) {
    window.on_open_settings({
        let weak = window.as_weak();
        move || {
            let Some(window) = weak.upgrade() else { return };
            let ui = window.global::<SettingsUi>();
            APP.with_borrow_mut(|app| {
                app.inputs = bardo_core::input_devices();
                app.outputs = bardo_core::output_devices();
                ui.set_threshold(app.settings.voice.threshold_db);
                ui.set_input_volume(f32::from(app.settings.voice.input_volume));
                ui.set_sounds_volume(f32::from(app.settings.voice.sounds_volume));
                ui.set_output_volume(f32::from(app.settings.voice.output_volume));
            });
            render_settings(&window);
            window.set_settings_open(true);
            let weak = window.as_weak();
            APP.with_borrow(|app| {
                app.meter.start(slint::TimerMode::Repeated, METER_INTERVAL, move || {
                    let Some(window) = weak.upgrade() else { return };
                    update_mic_test();
                    let ui = window.global::<SettingsUi>();
                    APP.with_borrow(|app| {
                        ui.set_level(app.transmit.level_db());
                        ui.set_transmitting(app.transmit.transmitting());
                        ui.set_in_voice(app.snapshot.voice.is_some());
                        let test = app.mic_test.as_ref();
                        ui.set_mic_test(match test.map(MicTest::phase) {
                            Some(MicTestPhase::Listening) => 1,
                            Some(MicTestPhase::Recording) => 2,
                            Some(MicTestPhase::Playing) => 3,
                            _ => 0,
                        });
                        ui.set_mic_test_progress(test.map_or(0.0, MicTest::progress));
                    });
                });
            });
        }
    });

    let ui = window.global::<SettingsUi>();
    ui.on_close({
        let weak = window.as_weak();
        move || {
            APP.with_borrow_mut(|app| {
                app.meter.stop();
                stop_mic_test(app);
                remember_gain(app);
            });
            if let Some(window) = weak.upgrade() {
                window.set_settings_open(false);
            }
        }
    });
    ui.on_select_input(|index| {
        APP.with_borrow_mut(remember_gain);
        update_voice_settings(|voice, app| voice.input_device = device_at(&app.inputs, index));
        // The test restarts on the new device, and automatic gain from what it learned for it.
        APP.with_borrow_mut(|app| {
            stop_mic_test(app);
            app.transmit.set_auto_gain_db(app.settings.voice.learned_gain());
        });
        send_devices();
        render_settings_now();
    });
    ui.on_select_output(|index| {
        update_voice_settings(|voice, app| voice.output_device = device_at(&app.outputs, index));
        APP.with_borrow_mut(stop_mic_test);
        send_devices();
        render_settings_now();
    });
    ui.on_start_mic_test(|kind| {
        let kind = match kind {
            1 => MicTestKind::Listen,
            2 => MicTestKind::Record,
            _ => MicTestKind::Meter,
        };
        APP.with_borrow_mut(|app| {
            // Let go of the microphone before opening it again.
            app.mic_test = None;
            app.mic_test_kind = kind;
        });
        update_mic_test();
    });
    ui.on_set_push_to_talk(|enabled| {
        update_voice_settings(|voice, _| voice.push_to_talk = enabled);
        render_settings_now();
    });
    ui.on_set_ptt_sounds(|enabled| {
        update_voice_settings(|voice, _| voice.push_to_talk_sounds = enabled);
        render_settings_now();
    });
    ui.on_set_threshold(|threshold| update_voice_settings(|voice, _| voice.threshold_db = threshold));
    ui.on_set_input_volume(|volume| {
        let volume = volume.round().clamp(0.0, f32::from(settings::MAX_INPUT_VOLUME)) as u16;
        update_voice_settings(|voice, _| voice.input_volume = volume);
    });
    ui.on_set_output_volume(|volume| {
        let volume = volume.round().clamp(0.0, f32::from(settings::MAX_VOLUME)) as u16;
        update_voice_settings(|voice, _| voice.output_volume = volume);
        send(Command::SetOutputGain(APP.with_borrow(|app| app.settings.voice.output_gain())));
    });
    ui.on_set_sounds_volume(|volume| {
        let volume = volume.round().clamp(0.0, f32::from(settings::MAX_SOUNDS_VOLUME)) as u16;
        update_voice_settings(|voice, _| voice.sounds_volume = volume);
        // A taste of the new volume; the sound gate keeps a drag from turning into a burst.
        play(Sound::UserJoin);
    });
    ui.on_set_noise_suppression(|enabled| {
        update_voice_settings(|voice, _| voice.noise_suppression = enabled);
        render_settings_now();
    });
    ui.on_set_auto_gain(|enabled| {
        update_voice_settings(|voice, _| voice.auto_gain = enabled);
        render_settings_now();
    });
    ui.on_capture_key(|target| {
        let Some(target) = KeyTarget::from_index(target) else { return };
        APP.with_borrow_mut(|app| app.capturing = Some(target));
        render_settings_now();
        APP.with_borrow(|app| {
            let Some(hotkeys) = &app.hotkeys else { return };
            hotkeys.capture_next(move |binding| {
                let _ = slint::invoke_from_event_loop(move || {
                    APP.with_borrow_mut(|app| app.capturing = None);
                    if binding.is_some() {
                        set_binding(target, binding);
                    }
                    render_settings_now();
                });
            });
        });
    });
    ui.on_clear_key(|target| {
        let Some(target) = KeyTarget::from_index(target) else { return };
        set_binding(target, None);
        render_settings_now();
    });
    ui.on_set_autostart(|enabled| {
        win::set_autostart(enabled);
        render_settings_now();
    });
    ui.on_set_sounds(|enabled| {
        update_voice_settings(|voice, _| voice.sounds = enabled);
        with_window(update_ringing);
        render_settings_now();
    });
    ui.on_open_logs(|| {
        let Some(dir) = logging::dir() else { return };
        if let Err(e) = std::process::Command::new("explorer.exe").arg(&dir).spawn() {
            warn!("failed to open {}: {e}", dir.display());
        }
    });
    ui.on_set_profile_pictures(|enabled| {
        update_settings(|settings| settings.appearance.profile_pictures = enabled);
        if !enabled {
            APP.with_borrow_mut(|app| app.pictures.clear());
        }
        render_all();
        render_settings_now();
    });
    ui.on_set_close_to_tray(|enabled| {
        update_settings(|settings| settings.window.close_to_tray = enabled);
        render_settings_now();
    });
    ui.on_sign_out({
        let weak = window.as_weak();
        move || {
            let Some(window) = weak.upgrade() else { return };
            let ephemeral = APP.with_borrow_mut(|app| {
                app.meter.stop();
                stop_mic_test(app);
                app.settings.ephemeral
            });
            window.set_settings_open(false);
            // The demo must not sign the real account out.
            if !ephemeral {
                token::clear();
            }
            disconnect(&window, "");
        }
    });
}

/// Signs in with a token, pasted or from a QR code.
fn sign_in(window: &MainWindow, input: &str) {
    if APP.with_borrow(|app| app.settings.ephemeral) {
        window.set_login_error("This is the demo: nothing is stored.".into());
        return;
    }
    let Some(token) = token::normalize(input) else {
        window.set_login_error("That doesn't look like a token.".into());
        return;
    };
    if let Err(e) = token::save(&token) {
        window.set_login_error(format!("Could not store the token: {e}").into());
        return;
    }
    connect(window, token);
}

/// Shows QR codes to sign in with until the sign-in screen goes away.
fn start_qr_login(window: &MainWindow) {
    window.set_qr_state(QR_WAITING);
    window.set_qr_status("".into());
    let (generation, demo) = APP.with_borrow_mut(|app| {
        app.qr_generation += 1;
        (app.qr_generation, app.settings.ephemeral)
    });
    if demo {
        on_qr_event(window, QrLoginEvent::Code("https://discord.com/ra/bardo-demo".into()));
        return;
    }
    let weak = window.as_weak();
    let login = QrLogin::start(move |event| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let current = APP.with_borrow(|app| app.qr_generation);
            if let (Some(window), true) = (weak.upgrade(), current == generation) {
                on_qr_event(&window, event);
            }
        });
    });
    APP.with_borrow_mut(|app| app.qr_login = Some(login));
}

fn stop_qr_login() {
    APP.with_borrow_mut(|app| {
        app.qr_login = None;
        app.qr_generation += 1;
    });
}

fn on_qr_event(window: &MainWindow, event: QrLoginEvent) {
    match event {
        QrLoginEvent::Code(code) => {
            match qr::image(&code) {
                Some(image) => {
                    window.set_qr_code(image);
                    window.set_qr_state(QR_SHOWN);
                }
                None => warn!("could not draw the sign-in QR code"),
            }
            window.set_qr_status("".into());
        }
        QrLoginEvent::Scanned { username } => {
            window.set_qr_state(QR_SCANNED);
            window.set_qr_status(format!("Confirm on your phone to sign in as {username}.").into());
        }
        QrLoginEvent::Token(token) => sign_in(window, &token),
        QrLoginEvent::Problem { message, retrying } => {
            window.set_qr_state(if retrying { QR_WAITING } else { QR_UNAVAILABLE });
            window.set_qr_status(message.into());
        }
    }
}

fn connect(window: &MainWindow, token: String) {
    stop_qr_login();
    window.set_login_error("".into());
    window.set_notice("".into());
    window.set_screen(SCREEN_MAIN);

    let generation = APP.with_borrow_mut(|app| {
        app.client = None;
        app.connection = Some(ConnectionState::Connecting);
        app.watched.clear();
        app.generation += 1;
        app.generation
    });
    render_status(window);
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
        client.send(Command::SetOutputGain(app.settings.voice.output_gain()));
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
        app.connection = None;
        app.ringer = None;
        app.ringing = None;
        app.ringback = None;
        app.generation += 1;
        app.snapshot = Arc::default();
        app.speaking.clear();
        app.list = ListState::default();
    });
    render_all();
    window.set_login_error(error.into());
    window.set_screen(SCREEN_LOGIN);
    start_qr_login(window);
}

fn on_client_event(window: &MainWindow, event: ClientEvent) {
    match event {
        ClientEvent::State(ConnectionState::AuthenticationFailed) => {
            token::clear();
            disconnect(window, "Discord rejected the token. Sign in again.");
        }
        ClientEvent::State(state) => {
            APP.with_borrow_mut(|app| app.connection = Some(state));
            render_status(window);
            render_tray();
        }
        ClientEvent::Snapshot(snapshot) => {
            APP.with_borrow_mut(|app| {
                if app.snapshot.voice.is_some() && snapshot.voice.is_none() {
                    remember_gain(app);
                }
                if snapshot.voice.is_none() {
                    let me = snapshot.me.as_ref().map(|(id, _)| *id);
                    app.speaking.retain(|id| Some(*id) == me);
                }
                let voice = &mut app.settings.voice;
                if (voice.self_mute, voice.self_deaf) != (snapshot.self_mute, snapshot.self_deaf) {
                    (voice.self_mute, voice.self_deaf) = (snapshot.self_mute, snapshot.self_deaf);
                    app.settings.save();
                }
                app.snapshot = snapshot;
            });
            update_ringing(window);
            render_all();
        }
        ClientEvent::Speaking(users) => {
            APP.with_borrow_mut(|app| app.speaking = users.into_iter().collect());
            update_speaking(window);
        }
        ClientEvent::TokenRefreshed(token) => {
            if let Err(e) = token::save(&token) {
                warn!("failed to store refreshed token: {e}");
            }
        }
        ClientEvent::Notice(message) => show_notice(window, message),
        ClientEvent::Sound(sound) => play(sound),
        ClientEvent::UpdateAvailable(release) => {
            // A reconnect checks again; a download under way carries on.
            if window.get_update_state() != UPDATE_DOWNLOADING {
                window.set_update_state(UPDATE_OFFERED);
            }
            window.set_update_version(release.version.as_str().into());
            APP.with_borrow_mut(|app| app.update = Some(release));
        }
    }
}

fn open_release_page() {
    let url = APP.with_borrow(|app| app.update.as_ref().map(|release| release.url.clone()));
    win::open(url.as_deref().unwrap_or(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/latest")));
}

/// Downloads the newer version, then closes bardo so it can take over.
fn install_update(window: &MainWindow) {
    let Some(release) = APP.with_borrow(|app| app.update.clone()) else {
        return open_release_page();
    };
    window.set_update_progress(0.0);
    window.set_update_state(UPDATE_DOWNLOADING);
    let package = update::package();
    let mut shown = 0;
    bardo_core::download_update(release, package, move |event| {
        // Only whole percents reach the UI.
        if let UpdateEvent::Progress(done) = event {
            let percent = (done * 100.0) as u32;
            if percent == shown {
                return;
            }
            shown = percent;
        }
        let _ = slint::invoke_from_event_loop(move || with_window(|window| on_update_event(window, package, event)));
    });
}

fn on_update_event(window: &MainWindow, package: Package, event: UpdateEvent) {
    match event {
        UpdateEvent::Progress(done) => window.set_update_progress(done),
        UpdateEvent::Downloaded(bytes) => match update::prepare(package, &bytes) {
            Ok(pending) => {
                APP.with_borrow_mut(|app| app.pending_update = Some(pending));
                let _ = slint::quit_event_loop();
            }
            Err(e) => {
                warn!("failed to put the update in place: {e}");
                window.set_update_state(UPDATE_FAILED);
            }
        },
        UpdateEvent::Failed(e) => {
            warn!("failed to download the update: {e}");
            window.set_update_state(UPDATE_FAILED);
        }
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

/// Joins a guild voice channel, or a DM or group call when `guild` is empty.
fn join(guild: &str, channel: &str) {
    let Some(channel_id) = parse_id(channel) else { return };
    let guild_id = if guild.is_empty() { None } else { parse_id(guild) };
    if guild_id.is_none() && !guild.is_empty() {
        return;
    }
    APP.with_borrow_mut(|app| app.ringer = None);
    send(Command::JoinVoice { guild_id, channel_id });
}

fn start_call(channel: &str) {
    if let Some(channel_id) = parse_id(channel) {
        send(Command::StartCall { channel_id });
    }
}

/// Runs the microphone test while the settings are open outside voice; in voice, the voice
/// connection's own capture feeds the meter.
fn update_mic_test() {
    APP.with_borrow_mut(|app| {
        let test = app.mic_test.as_ref();
        if app.snapshot.voice.is_some()
            || test.is_some_and(MicTest::is_lost)
            || test.is_some_and(|test| test.phase() == MicTestPhase::Finished)
        {
            // In voice, a device went away or the recording was played back: back to the meter,
            // opened again on the next tick.
            stop_mic_test(app);
        } else if app.mic_test.is_none() {
            let voice = &app.settings.voice;
            let (input, output) = (voice.input_device.as_deref(), voice.output_device.as_deref());
            match MicTest::start(app.mic_test_kind, input, output, app.transmit.clone()) {
                Ok(test) => app.mic_test = Some(test),
                Err(e) if app.mic_test_kind != MicTestKind::Meter => {
                    warn!("could not start the microphone test: {e}");
                    app.mic_test_kind = MicTestKind::Meter;
                }
                // Retried on the next tick; say it once.
                Err(e) if app.meter.running() => {
                    warn!("could not open the microphone for the level meter: {e}");
                    app.meter.stop();
                }
                Err(_) => {}
            }
        }
    });
}

/// Stops listening or recording; the meter comes back on the next tick if the settings are open.
fn stop_mic_test(app: &mut App) {
    app.mic_test = None;
    app.mic_test_kind = MicTestKind::Meter;
}

/// Starts or stops the ringtone, and shows who is calling.
fn update_ringing(window: &MainWindow) {
    let (calling, deaf, sounds, output) = APP.with_borrow(|app| {
        let in_voice = app.snapshot.voice.as_ref().map(|voice| voice.channel_id);
        let calling = app.snapshot.calls.iter().find(|call| call.ringing_me && in_voice != Some(call.channel_id));
        let voice = &app.settings.voice;
        (calling.cloned(), app.snapshot.self_deaf, voice.sounds, (voice.output_device.clone(), voice.sounds_gain()))
    });
    window.set_incoming(match &calling {
        Some(call) => IncomingCall {
            active: true,
            channel_id: call.channel_id.to_string().into(),
            name: call.name.as_str().into(),
            initial: view::initial(&call.name).into(),
            group: call.group,
        },
        None => IncomingCall::default(),
    });
    let channel = calling.as_ref().map(|call| call.channel_id);
    let is_new = APP.with_borrow_mut(|app| {
        let is_new = channel.is_some() && app.ringing != channel;
        app.ringing = channel;
        if channel.is_none() || !sounds || deaf {
            app.ringer = None;
        } else if is_new {
            app.ringer = Some(Ringer::start(output.0.clone(), output.1));
        }
        update_ringback(app, sounds && !deaf, output);
        is_new
    });
    if is_new {
        win::flash(window.window());
    }
}

/// Plays the ringback while a call we are in rings the others, for up to [`RINGBACK_LIMIT`].
/// `output`: the device and the volume.
fn update_ringback(app: &mut App, audible: bool, output: (Option<String>, f32)) {
    let call = bardo_core::cues::ringback(&app.snapshot);
    if app.ringback.as_ref().map(|ringback| ringback.call) != call {
        app.ringback = call.map(|call| {
            // Look again once the time is up.
            slint::Timer::single_shot(RINGBACK_LIMIT + Duration::from_millis(100), || with_window(update_ringing));
            Ringback { call, since: Instant::now(), ringer: None }
        });
    }
    let Some(ringback) = &mut app.ringback else { return };
    let audible = audible && ringback.since.elapsed() < RINGBACK_LIMIT;
    if audible != ringback.ringer.is_some() {
        ringback.ringer = audible.then(|| Ringer::ringback(output.0, output.1));
    }
}

/// Plays `sound` on the chosen output, if sounds are on and it gets past the [`SoundGate`].
fn play(sound: Sound) {
    let output = APP.with_borrow_mut(|app| {
        let (deaf, voice) = (app.snapshot.self_deaf, &app.settings.voice);
        let admitted = voice.sounds && app.sound_gate.admit(sound, deaf, Instant::now());
        admitted.then(|| (voice.output_device.clone(), voice.sounds_gain()))
    });
    if let Some((device, volume)) = output {
        debug!(?sound, "playing");
        bardo_core::play_sound(sound, device, volume);
    }
}

/// Feeds the sound demo's steps through the same path as the client's events.
#[cfg(debug_assertions)]
fn run_sound_demo() -> slint::Timer {
    let steps = RefCell::new(demo::sound_script().into_iter());
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(1200), move || {
        let Some(step) = steps.borrow_mut().next() else { return };
        with_window(|window| match step {
            demo::Step::Sound(sound) => on_client_event(window, ClientEvent::Sound(sound)),
            demo::Step::Snapshot(snapshot) => {
                let previous = APP.with_borrow(|app| app.snapshot.clone());
                for sound in bardo_core::cues::between(&previous, &snapshot) {
                    on_client_event(window, ClientEvent::Sound(sound));
                }
                on_client_event(window, ClientEvent::Snapshot(Arc::new(*snapshot)));
            }
            demo::Step::Wait => {}
        });
    });
    timer
}

#[cfg(debug_assertions)]
fn run_tour(steps: Rc<RefCell<std::vec::IntoIter<(demo::TourStep, Duration)>>>) {
    let Some((step, duration)) = steps.borrow_mut().next() else { return };
    with_window(|window| match step {
        demo::TourStep::Snapshot(snapshot) => on_client_event(window, ClientEvent::Snapshot(Arc::new(*snapshot))),
        demo::TourStep::Speaking(users) => {
            on_client_event(window, ClientEvent::Speaking(users.iter().map(|&id| Id(id)).collect()));
        }
        demo::TourStep::Search(query) => {
            window.set_search(query.into());
            window.invoke_search_edited(query.into());
        }
    });
    slint::Timer::single_shot(duration, move || run_tour(steps));
}

fn push_to_talk_sound(pressed: bool) {
    let wanted = APP.with_borrow(|app| {
        let voice = &app.settings.voice;
        let connected = app.snapshot.voice.as_ref().is_some_and(|voice| voice.status == VoiceStatus::Connected);
        voice.push_to_talk && voice.push_to_talk_sounds && connected && !app.snapshot.self_mute
    });
    if wanted {
        play(if pressed { Sound::PushToTalkOn } else { Sound::PushToTalkOff });
    }
}

fn toggle_mute() {
    let (connected, mute, deaf) =
        APP.with_borrow(|app| (app.client.is_some(), app.snapshot.self_mute, app.snapshot.self_deaf));
    if !connected {
        return;
    }
    // Like the official client: unmuting while deafened also undeafens.
    if deaf {
        send(Command::SetSelfDeaf(false));
        send(Command::SetSelfMute(false));
        play(Sound::Unmute);
    } else {
        send(Command::SetSelfMute(!mute));
        play(if mute { Sound::Unmute } else { Sound::Mute });
    }
}

fn toggle_deafen() {
    let (connected, deaf) = APP.with_borrow(|app| (app.client.is_some(), app.snapshot.self_deaf));
    if !connected {
        return;
    }
    send(Command::SetSelfDeaf(!deaf));
    play(if deaf { Sound::Undeafen } else { Sound::Deafen });
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

fn update_settings(update: impl FnOnce(&mut Settings)) {
    APP.with_borrow_mut(|app| {
        update(&mut app.settings);
        app.settings.save();
    });
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

/// Keeps what automatic gain control learned for the microphone, for the next session.
fn remember_gain(app: &mut App) {
    if app.settings.voice.remember_gain(app.transmit.auto_gain_db()) {
        app.settings.save();
    }
}

fn apply_voice_settings(transmit: &TransmitControl, voice: &VoiceSettings) {
    transmit.set_push_to_talk(voice.push_to_talk);
    transmit.set_threshold_db(voice.threshold_db);
    transmit.set_noise_suppression(voice.noise_suppression);
    transmit.set_input_gain(voice.input_gain());
    transmit.set_auto_gain(voice.auto_gain);
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

fn audio_devices(voice: &VoiceSettings) -> AudioDevices {
    AudioDevices { input: voice.input_device.clone(), output: voice.output_device.clone() }
}

fn send_devices() {
    let devices = APP.with_borrow(|app| audio_devices(&app.settings.voice));
    send(Command::SetAudioDevices(devices));
}

/// The device at a settings dropdown index; index 0 is "System default".
fn device_at(devices: &[AudioDevice], index: i32) -> Option<String> {
    let index = usize::try_from(index).ok()?.checked_sub(1)?;
    devices.get(index).map(|device| device.id.clone())
}

fn user_audio(audio: settings::UserAudio) -> UserAudio {
    UserAudio { volume: audio.volume, muted: audio.muted }
}

fn parse_id(id: &str) -> Option<Id> {
    id.parse().ok()
}

fn strings(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    let items: Vec<SharedString> = items.into_iter().map(SharedString::from).collect();
    ModelRc::new(VecModel::from(items))
}

fn with_window(f: impl FnOnce(&MainWindow)) {
    let window = APP.with_borrow(|app| app.window.as_ref().and_then(slint::Weak::upgrade));
    if let Some(window) = window {
        f(&window);
    }
}

fn render_settings_now() {
    with_window(render_settings);
}

fn render_settings(window: &MainWindow) {
    let view = APP.with_borrow(|app| {
        let voice = &app.settings.voice;
        let names = |devices: &[AudioDevice]| {
            strings(std::iter::once("System default".to_owned()).chain(devices.iter().map(|d| d.name.clone())))
        };
        let index = |devices: &[AudioDevice], id: &Option<String>| {
            id.as_ref()
                .and_then(|id| devices.iter().position(|device| &device.id == id))
                .map_or(0, |position| position as i32 + 1)
        };
        let keys = |binding: Option<Binding>| strings(binding.map(Binding::parts).unwrap_or_default());
        SettingsView {
            inputs: names(&app.inputs),
            outputs: names(&app.outputs),
            input_index: index(&app.inputs, &voice.input_device),
            output_index: index(&app.outputs, &voice.output_device),
            push_to_talk: voice.push_to_talk,
            ptt_sounds: voice.push_to_talk_sounds,
            ptt_keys: strings(voice.push_to_talk_key.map(hotkey::key_name)),
            mute_keys: keys(app.settings.hotkeys.toggle_mute),
            deafen_keys: keys(app.settings.hotkeys.toggle_deafen),
            capturing: app.capturing.map_or(-1, |target| target as i32),
            noise_suppression: voice.noise_suppression,
            auto_gain: voice.auto_gain,
            sounds: voice.sounds,
            profile_pictures: app.settings.appearance.profile_pictures,
            autostart: win::autostart_enabled(),
            close_to_tray: app.settings.window.close_to_tray,
            account: app.snapshot.me.as_ref().map(|(_, name)| name.as_str()).unwrap_or_default().into(),
            version: concat!("bardo ", env!("CARGO_PKG_VERSION")).into(),
        }
    });
    window.global::<SettingsUi>().set_view(view);
}

/// Everything that depends on the snapshot.
fn render_all() {
    render();
    with_window(|window| {
        render_voice(window);
        render_status(window);
    });
    render_tray();
}

/// Rebuilds the list, and tells the client which guilds are on screen.
fn render() {
    let (mut rows, watched) = APP.with_borrow(|app| {
        let cx = Context { snapshot: &app.snapshot, settings: &app.settings, speaking: &app.speaking, list: &app.list };
        (view::rows(&cx), view::watched_guilds(&cx))
    });
    APP.with_borrow_mut(|app| {
        for row in rows.iter_mut().filter(|row| !row.picture_path.is_empty()) {
            if let Some(picture) = app.pictures.get(&row.picture_path, picture_arrived) {
                row.picture = picture;
            }
        }
        app.rows.set_vec(rows);
    });
    let changed = APP.with_borrow_mut(|app| {
        let changed = app.watched != watched;
        app.watched = watched.clone();
        changed
    });
    if changed {
        send(Command::SetWatchedGuilds(watched));
    }
}

/// A picture was fetched: shows it wherever it is wanted.
fn picture_arrived(path: String, pixels: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>) {
    let Some(picture) = APP.with_borrow_mut(|app| app.pictures.arrived(path.clone(), pixels)) else { return };
    APP.with_borrow(|app| {
        for index in 0..app.rows.row_count() {
            let Some(mut row) = app.rows.row_data(index) else { continue };
            if row.picture_path == path {
                row.picture = picture.clone();
                app.rows.set_row_data(index, row);
            }
        }
    });
    with_window(render_me_picture);
}

/// Our own picture in the user panel.
fn render_me_picture(window: &MainWindow) {
    let picture = APP.with_borrow_mut(|app| {
        let path = app.snapshot.me_avatar.as_ref().filter(|_| app.settings.show_pictures())?.path.clone();
        app.pictures.get(&path, picture_arrived)
    });
    window.set_me_picture(picture.unwrap_or_default());
}

/// Refreshes only the speaking flags, which change several times a second.
fn update_speaking(window: &MainWindow) {
    APP.with_borrow(|app| {
        for index in 0..app.rows.row_count() {
            let Some(mut row) = app.rows.row_data(index) else { continue };
            if !matches!(row.kind, RowKind::Participant | RowKind::Person) {
                continue;
            }
            let speaking = row.current && parse_id(&row.id).is_some_and(|id| app.speaking.contains(&id));
            if row.speaking != speaking {
                row.speaking = speaking;
                app.rows.set_row_data(index, row);
            }
        }
        let me = app.snapshot.me.as_ref().map(|(id, _)| *id);
        window.set_me_speaking(me.is_some_and(|me| app.speaking.contains(&me)));
    });
}

fn render_status(window: &MainWindow) {
    APP.with_borrow(|app| {
        let status = match &app.connection {
            None | Some(ConnectionState::Connecting) => "Connecting…".to_owned(),
            Some(ConnectionState::Connected) => "Connected".to_owned(),
            Some(ConnectionState::Reconnecting { attempt }) => format!("Reconnecting (attempt {attempt})…"),
            Some(ConnectionState::Failed(reason)) => format!("Offline: {reason}"),
            Some(ConnectionState::AuthenticationFailed) => "Signed out".to_owned(),
        };
        window.set_status(status.into());
        window.set_connected(app.connection == Some(ConnectionState::Connected));
        let name = app.snapshot.me.as_ref().map(|(_, name)| name.as_str()).unwrap_or("bardo");
        window.set_me_name(name.into());
        window.set_me_initial(view::initial(name).into());
        window.set_self_mute(app.snapshot.self_mute);
        window.set_self_deaf(app.snapshot.self_deaf);
    });
    render_me_picture(window);
}

fn render_voice(window: &MainWindow) {
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
