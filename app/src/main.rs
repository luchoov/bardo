#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod token;

use std::{cell::RefCell, collections::HashSet, rc::Rc, sync::Arc};

use bardo_core::{Client, ClientEvent, ConnectionState, Id, Snapshot};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::warn;
use tracing_subscriber::EnvFilter;

slint::include_modules!();

const SCREEN_LOGIN: i32 = 0;
const SCREEN_MAIN: i32 = 1;

/// UI-thread state.
#[derive(Default)]
struct App {
    client: Option<Client>,
    /// Bumped on every connect and sign-out so events from a previous client are ignored.
    generation: u64,
    snapshot: Arc<Snapshot>,
    collapsed: HashSet<Id>,
    rows: Rc<VecModel<Row>>,
}

thread_local! {
    static APP: RefCell<App> = RefCell::default();
}

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    token::init();

    let window = MainWindow::new()?;
    window.set_rows(ModelRc::from(APP.with_borrow(|app| app.rows.clone())));

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
        let Ok(id) = id.parse::<Id>() else { return };
        APP.with_borrow_mut(|app| {
            if !app.collapsed.remove(&id) {
                app.collapsed.insert(id);
            }
        });
        render();
    });

    match token::load() {
        Some(token) => connect(&window, token),
        None => window.set_screen(SCREEN_LOGIN),
    }
    window.run()
}

fn connect(window: &MainWindow, token: String) {
    window.set_login_error("".into());
    window.set_screen(SCREEN_MAIN);
    window.set_connected(false);
    window.set_status("Connecting…".into());

    let generation = APP.with_borrow_mut(|app| {
        app.client = None;
        app.generation += 1;
        app.generation
    });
    let weak = window.as_weak();
    let client = Client::start(token, move |event| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let current = APP.with_borrow(|app| app.generation);
            if let (Some(window), true) = (weak.upgrade(), current == generation) {
                on_client_event(&window, event);
            }
        });
    });
    APP.with_borrow_mut(|app| app.client = Some(client));
}

fn disconnect(window: &MainWindow, error: &str) {
    APP.with_borrow_mut(|app| {
        app.client = None;
        app.generation += 1;
        app.snapshot = Arc::default();
        app.collapsed.clear();
    });
    render();
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
        }
        ClientEvent::Snapshot(snapshot) => {
            APP.with_borrow_mut(|app| app.snapshot = snapshot);
            render();
        }
        ClientEvent::TokenRefreshed(token) => {
            if let Err(e) = token::save(&token) {
                warn!("failed to store refreshed token: {e}");
            }
        }
    }
}

/// Flattens the snapshot into list rows, honoring collapsed guilds.
fn render() {
    APP.with_borrow(|app| {
        let mut rows = Vec::new();
        for guild in &app.snapshot.guilds {
            let expanded = !app.collapsed.contains(&guild.id);
            rows.push(Row {
                kind: RowKind::Guild,
                id: guild.id.to_string().into(),
                label: guild.name.as_str().into(),
                expanded,
                ..Default::default()
            });
            if !expanded {
                continue;
            }
            for channel in &guild.channels {
                rows.push(Row {
                    kind: RowKind::Channel,
                    id: channel.id.to_string().into(),
                    label: channel.name.as_str().into(),
                    count: channel.participants.len() as i32,
                    limit: channel.user_limit as i32,
                    ..Default::default()
                });
                for participant in &channel.participants {
                    rows.push(Row {
                        kind: RowKind::Participant,
                        id: participant.user_id.to_string().into(),
                        label: participant.name.as_str().into(),
                        initial: initial(&participant.name).into(),
                        self_mute: participant.self_mute,
                        self_deaf: participant.self_deaf,
                        server_mute: participant.server_mute,
                        server_deaf: participant.server_deaf,
                        live: participant.streaming,
                        ..Default::default()
                    });
                }
            }
        }
        app.rows.set_vec(rows);
    });
}

fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}
