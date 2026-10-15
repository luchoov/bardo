# bardo

**An ultra-lightweight, voice-only Discord client for gaming.**

Playing CS2 (or anything else) and only need your squad in your ears? bardo does one thing: voice channels.
No Electron, no embedded browser, no chat, no feeds — a small native Rust binary for Windows 11 that stays
out of the way of your game's frame time and memory.

- See where your friends are talking, across all your servers, and join them in one click.
- Pin the servers you use; the rest stay folded away, and so do empty channels. Ctrl+K searches
  every channel and everyone in voice.
- Push-to-talk, or voice activity with noise suppression and automatic gain control.
- Global hotkeys for mute and deafen that do not take the keys away from the game.
- Per-user volume (0–200 %) and local mute; server mute, deafen, move and disconnect for admins.
- DM and group calls: call from search (Ctrl+K) or a friend's menu, answer or decline incoming ones.
- Lives in the tray and can start with Windows.
- Short, quiet sounds: mute and deafen (also when an admin sets them), joining, leaving, being
  moved or disconnected, people coming and going in your channel or call, the ringtone and the
  ringback of calls, and optionally a click on push-to-talk. Silent while you are deafened, except
  for your own mute and deafen.
- Runs alongside the official client: keep Discord closed while you play, or use it for text.

> **Status:** early development. Server voice works end to end (with DAVE): joining, listening,
> talking, moderation. DM and group calls (call, answer, decline, ring) are implemented but not yet
> verified against Discord.

> **Disclaimer:** bardo is not affiliated with or endorsed by Discord. Using a user account from an
> unofficial client is against Discord's Terms of Service and may get the account banned. Use at your own risk.

## Scope

**MVP**
- Token login (stored in Windows Credential Manager).
- Servers → voice channels → who is in each one (with their mute/deafen state).
- DMs and group DMs: incoming calls (ring), answer, call, hang up.
- Join / leave / switch channels. Speaking indicator.
- Self mute / deafen. Push-to-talk with a global hotkey, or VAD with a threshold.
- Input and output device selection, hot-swappable.
- Noise suppression (RNNoise).
- Context menu on a user:
  - Local: volume 0–200 %, local mute.
  - Moderation (based on effective permissions): server mute, server deafen, move to ▸, disconnect.
- System tray, global mute/deafen hotkeys, start with Windows.
- Voice sounds, synthesized: own mute/deafen, server mute/deafen, join, leave, moved, disconnected;
  others joining and leaving our channel or call; ringtone, ringback and call ended; push-to-talk
  (off by default). Like the official client, nothing plays when others mute or deafen.

**Out of scope**: text, video, screen share / Go Live, soundboard (the event is never handled, so it never plays), Stage.
No echo cancellation: headphones are assumed.

## Stack

| Piece | Choice |
|---|---|
| Language | Rust (edition 2024) |
| UI | Slint |
| Main gateway | Custom (`tokio-tungstenite`, `zlib-stream`); user-account identify, not bot |
| Client identity | Discord web client on Chrome/Windows; Chrome version and build number fetched at startup |
| Voice (voice WS, UDP/RTP, encryption, DAVE, Opus) | `songbird` 0.6, `driver` + `receive` only (DAVE via `davey`) |
| Audio I/O | WASAPI, through `cpal` |
| DSP | `nnnoiseless` (RNNoise), custom VAD |

DAVE (voice E2EE) has been mandatory for every call since March 1, 2026.

## Layout

```
crates/
  gateway/  Main WS: identify, heartbeat, resume, READY, voice states, op 4
  api/      Minimal REST: moderation (PATCH member), DM calls, member data
  voice/    Voice connection on top of songbird
  audio/    WASAPI capture/playback, mixing, per-user volume, RNNoise, VAD
  core/     State + effective permissions + orchestration; command/event API for the UI
app/        Slint binary: view, global hotkeys, tray
tools/      make_icon.py draws the icon
```

Threads: a tokio runtime for networking, dedicated real-time threads for audio, the Slint event loop on the main thread; channels in between.

## Roadmap

1. Gateway: servers and voice channels with who is in each, live. *(done)*
2. Moderation: context menu (mute/deafen/move/disconnect). *(done)*
3. Voice — receive: join and listen (validates DAVE). *(done)*
4. Voice — send: microphone, PTT, VAD, input gain. *(done)*
5. DM / group calls. *(implemented: ongoing calls listed, incoming call card with a ringtone,
   calling rings the others; needs a live test)*
6. Polish: tray, global hotkeys, own UI components, friends in voice, pinned servers, search. *(done)*
7. Release build and installer; memory and CPU numbers against the official client.

## Build

Requirements: Rust stable (MSVC), Visual Studio Build Tools with C++ and the Windows SDK, CMake.

```
cargo run -p bardo
cargo test --workspace
cargo run -p bardo-audio --example tone   # plays a short tone to check audio output
cargo run -p bardo-audio --example mic    # prints the microphone level for a few seconds
```

## Signing in

bardo asks for your account token and stores it in Windows Credential Manager (`bardo` / `discord-token`).
To get it, open Discord in a browser, open DevTools → Network, click any request to `discord.com/api`,
and copy the `Authorization` request header.

The token grants full access to the account: never share it or commit it.

## Development

| Variable | Effect |
|---|---|
| `BARDO_TOKEN` | Use this token instead of the stored one. |
| `BARDO_DUMP_DIR` | Write raw `READY`, `READY_SUPPLEMENTAL` and unparseable dispatches to this directory. They contain account data: keep the directory outside the repo. |
| `RUST_LOG` | Log filter, e.g. `info,bardo_gateway=debug`. Debug builds log to the console. `songbird=debug` shows the voice handshake. |
| `BARDO_DEMO` | Debug builds only: show made-up servers instead of connecting, to work on the UI without an account (`login` shows the sign-in screen, `call` an incoming call, `sounds` people coming and going and a call we make, to hear every sound). Nothing is saved. |

Settings (voice, hotkeys, pinned servers, per-user volume and local mutes) are stored in
`%APPDATA%\bardo\settings.json`.

Hotkeys are read by polling the keyboard state, so the keys still reach the game.

Member data (names) is only requested for the servers on screen, so big public servers cost nothing
while they are folded away.

## References

- [docs.discord.food](https://docs.discord.food): unofficial documentation of the user-account API.
- [Abaddon](https://github.com/uowuo/abaddon): native C++ client with voice and DAVE.
- [discord.py-self](https://github.com/dolfies/discord.py-self): reference for client identity and gateway capabilities.
