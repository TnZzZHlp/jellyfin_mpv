# jellyfin_mpv

Jellyfin cast receiver using external mpv. Registers as a remote playback device, receives WebSocket commands from Jellyfin, and controls mpv via JSON IPC.

## Build & Run

```powershell
cargo build --release
cargo run --release
```

Requires `mpv` in PATH or set via `--mpv-path`.

## Required Validation

Tests are inline in source files under `src/`. Before considering any source change complete, run:

```powershell
cargo fmt --all
cargo lint
cargo test
```

`cargo lint` is mandatory. It must finish with no warnings or errors; fix every diagnostic before completion and inspect any automatic fixes it applies. The repository does not define this Cargo alias, so do not silently skip the check if the active environment lacks it.

For changes touching Windows-only code, also verify the MinGW target when working from another host:

```powershell
cargo +stable clippy --target x86_64-pc-windows-gnu --all-targets --all-features
cargo +stable test --target x86_64-pc-windows-gnu --all-features --no-run
```

## Toolchain

Active toolchain is `stable-x86_64-pc-windows-gnu` (MinGW). MSVC toolchain requires Visual Studio C++ Build Tools + Windows SDK. If building with MSVC fails with `cannot open input file 'kernel32.lib'`, either install the missing components or switch to MinGW:

```powershell
rustup default stable-x86_64-pc-windows-gnu
scoop install gcc
```

## Key Architecture

- `src/main.rs`: Entry point, single-instance check, tray icon, and connection loop
- `src/jellyfin.rs`: HTTP/WebSocket client, authentication, media info, and progress reporting
- `src/player.rs`: Playback state machine, mpv IPC, bitrate adaptation, and media segment skipping
- `src/mpv.rs`: mpv process spawning and JSON IPC protocol
- `src/protocol.rs`: Jellyfin API types and WebSocket message parsing
- `src/config.rs`: CLI args and environment variable handling
- `src/tray.rs`: Windows system tray icon and menu
- `src/login_ui.rs` / `src/settings_ui.rs`: Windows dialogs
- `src/credential_store.rs`: Windows Credential Manager for saved logins
- `src/skip_settings.rs`: Persisted playback and media segment preferences
- `src/thumbnail.rs`: Jellyfin Trickplay selection, caching, and mpv overlays
- `src/quality_menu.lua` / `src/thumbfast.lua`: Embedded mpv-side bridge scripts
- `src/single_instance.rs`: Windows single-instance guard
- `README.md`: User-facing behavior, setup, and operational documentation

## Coding Conventions and Constraints

- Use Rust 2024 and existing Tokio async patterns; use `anyhow` for contextual errors and `tracing` for logs.
- Keep tests beside the implementation in `#[cfg(test)]` modules and add focused regression coverage for non-trivial behavior changes.
- Preserve `#[cfg(windows)]` Win32 implementations and their non-Windows fallbacks; lint the affected platform rather than assuming a host-only check covers both.
- Match existing Chinese user-facing messages and logs.
- Treat passwords, access tokens, and authenticated headers as secrets. Never place them in logs, URLs, fixtures, or documentation.
- Do not use emoji anywhere in the repository, including code, comments, tests, documentation, configuration, or commit messages.

## CLI / Environment

All login parameters accept CLI flags or environment variables:

| Flag | Env var | Notes |
|------|---------|-------|
| `--server` | `JELLYFIN_SERVER` | Jellyfin URL |
| `--username` | `JELLYFIN_USERNAME` | |
| `--password` | `JELLYFIN_PASSWORD` | **Base64-encoded UTF-8** |
| `--device-id` | `JELLYFIN_DEVICE_ID` | Must be stable across restarts |
| `--device-name` | `JELLYFIN_DEVICE_NAME` | Display name in Jellyfin |
| `--mpv-path` | `MPV_PATH` | |
| `--no-ui` | `JELLYFIN_NO_UI` | Fail if login params missing |

Password is Base64-encoded on CLI/env but plaintext in the login dialog. Example:

```powershell
$passwordBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("your-password"))
cargo run --release -- --server "http://jellyfin.local:8096" --username "user" --password $passwordBase64
```

## Data Locations

- Logs: `%LOCALAPPDATA%\jellyfin_mpv\jellyfin_mpv.log`
- Settings: `%LOCALAPPDATA%\jellyfin_mpv\settings.json`
- Credentials: Windows Credential Manager, target `jellyfin_mpv/login`

## Debug Logging

```powershell
$env:RUST_LOG = "jellyfin_mpv=debug"
cargo run --release
```

## Edition

Uses Rust edition 2024. Requires Rust 1.85+.
