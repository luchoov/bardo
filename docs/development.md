# Development

## Building

Requirements: Rust stable (MSVC), Visual Studio Build Tools with C++ and the Windows SDK, and CMake
(for Opus).

```
cargo run -p bardo
cargo test --workspace
cargo clippy --workspace --all-targets
cargo build --release -p bardo                 # target\release\bardo.exe
cargo run -p bardo-audio --example tone        # plays a short tone to check audio output
cargo run -p bardo-audio --example mic         # prints the microphone level for a few seconds
```

The C runtime is linked statically (`.cargo/config.toml`), so the exe runs without the Visual C++
Redistributable. The code is formatted by hand at about 120 columns; `cargo fmt` would rewrite it.

## Layout

```
crates/
  gateway/  Main WebSocket: identify, heartbeat, resume, READY, voice states, op 4
  api/      Minimal REST: moderation (PATCH member), DM calls, member data, bardo's own releases;
            QR code sign-in (remote authentication gateway)
  voice/    Voice connection on top of songbird
  audio/    WASAPI capture and playback, mixing, per-user volume, RNNoise, VAD, sounds
  core/     State, effective permissions and orchestration; a command/event API for the UI
app/        Slint binary: view, global hotkeys, tray
installer/  Inno Setup script for bardo-setup.exe
tools/      Icon, screenshots, benchmark
```

Threads: a tokio runtime for networking, dedicated real-time threads for audio, and the Slint event
loop on the main thread, with channels in between.

## Stack

| Piece | Choice |
|---|---|
| Language | Rust (edition 2024) |
| UI | Slint |
| Main gateway | Own client (`tokio-tungstenite`, `zlib-stream`); user-account identify, not bot |
| Client identity | Discord web client on Chrome/Windows; Chrome version and build number fetched at startup |
| Voice (voice WS, UDP/RTP, encryption, DAVE, Opus) | `songbird` 0.6, `driver` and `receive` only (DAVE via `davey`) |
| Audio I/O | WASAPI, through `cpal` |
| DSP | `nnnoiseless` (RNNoise), own VAD |

DAVE (voice end-to-end encryption) has been mandatory for every call since March 1, 2026.

Member data (names) is only requested for the servers on screen, so big public servers cost nothing
while they are folded away. Hotkeys are read by polling the keyboard state, so the keys still reach
the game.

## Environment variables

| Variable | Effect |
|---|---|
| `BARDO_TOKEN` | Use this token instead of the stored one. |
| `BARDO_DUMP_DIR` | Write raw `READY`, `READY_SUPPLEMENTAL` and unparseable dispatches to this directory. They contain account data: keep the directory outside the repo. |
| `RUST_LOG` | Log filter, e.g. `info,bardo_gateway=debug`. Debug builds log to the console, release builds to `%LOCALAPPDATA%\bardo\bardo.log`. `songbird=debug` shows the voice handshake. |
| `BARDO_DEMO` | Debug builds only: made-up servers and people instead of an account. Nothing is saved. Scenes: `1` (the list), `login`, `call` (an incoming call), `search`, `settings`, `update` (the new-version bar), `tour` (people join and talk, a search is typed), `sounds` (plays every sound). |

Settings (voice, hotkeys, pinned servers, per-user volume and local mutes) are stored in
`%APPDATA%\bardo\settings.json`; the token in Windows Credential Manager (`bardo` / `discord-token`).

## Tools

- `tools/screenshots.ps1` takes the README images from the demo and records `tour.gif` (needs a
  debug build, and Python with Pillow).
- `tools/bench.ps1` measures bardo and Discord side by side while you use them, and summarizes a
  run as a Markdown table.
- `tools/make_icon.py` draws the icon.

## Releasing

1. Bump `version` in the root `Cargo.toml` and commit.
2. `git tag -a v0.2.0 -m "Release notes"` and `git push origin v0.2.0`.

The release workflow checks that the tag matches the version, builds the exe and the installer, and
publishes both with their SHA-256 checksums. The tag's message becomes the release notes. Running
copies of bardo offer the new version the next time they sign in, and "Update" installs it: an
installed bardo runs the new installer silently, a portable one swaps its own exe. Both start again
when done. Keep the asset names, since bardo looks for them by name.

## References

- [docs.discord.food](https://docs.discord.food): unofficial documentation of the user-account API.
- [Abaddon](https://github.com/uowuo/abaddon): native C++ client with voice and DAVE.
- [discord.py-self](https://github.com/dolfies/discord.py-self): reference for client identity and
  gateway capabilities.
