# bardo

**An ultra-lightweight, voice-only Discord client for gaming.**

Playing CS2 (or anything else) and only need your squad in your ears? bardo does one thing: voice channels.
No Electron, no embedded browser, no chat, no feeds — a small native Rust binary for Windows 11 that stays
out of the way of your game's frame time and memory.

- Join voice channels, see who is in them and who is talking.
- Push-to-talk on a global hotkey, or voice activation.
- Server mute, deafen, move and disconnect from a right-click menu, for server admins.
- Runs alongside the official client: keep Discord closed while you play, or use it for text.

> **Status:** early development. Server and voice channel listing works; joining voice is not implemented yet.

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
- System tray.

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
| Audio I/O | WASAPI |
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
```

Threads: a tokio runtime for networking, dedicated real-time threads for audio, the Slint event loop on the main thread; channels in between.

## Roadmap

1. Gateway: servers and voice channels with who is in each, live. *(in progress: needs a live test)*
2. Moderation: context menu (mute/deafen/move/disconnect). Useful without audio.
3. Voice — receive: join and listen (validates DAVE).
4. Voice — send: microphone, PTT, VAD.
5. DM / group calls.
6. Polish: RNNoise, per-user volume, hotkeys, tray.

## Build

Requirements: Rust stable (MSVC), Visual Studio Build Tools with C++ and the Windows SDK, CMake.

```
cargo run -p bardo
cargo test --workspace
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
| `RUST_LOG` | Log filter, e.g. `info,bardo_gateway=debug`. Debug builds log to the console. |

## References

- [docs.discord.food](https://docs.discord.food): unofficial documentation of the user-account API.
- [Abaddon](https://github.com/uowuo/abaddon): native C++ client with voice and DAVE.
- [discord.py-self](https://github.com/dolfies/discord.py-self): reference for client identity and gateway capabilities.
