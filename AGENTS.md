# jellyfin_mpv

Jellyfin cast receiver using external mpv. Registers as a remote playback device, receives WebSocket commands from Jellyfin, and controls mpv via JSON IPC.

## Build & Run

```powershell
cargo build --release
cargo run --release
```

Requires `mpv` in PATH or set via `--mpv-path`.

## Tests

Tests are inline in source files under `src/`. Run with:

```powershell
cargo test
```

## Toolchain

Active toolchain is `stable-x86_64-pc-windows-gnu` (MinGW). MSVC toolchain requires Visual Studio C++ Build Tools + Windows SDK. If building with MSVC fails with `cannot open input file 'kernel32.lib'`, either install the missing components or switch to MinGW:

```powershell
rustup default stable-x86_64-pc-windows-gnu
scoop install gcc
```

## Key Architecture

- `main.rs`: Entry point, single-instance check, tray icon, connection loop
- `jellyfin.rs`: HTTP/WebSocket client, authentication, media info, progress reporting
- `player.rs`: Playback state machine, mpv IPC, media segment skipping
- `mpv.rs`: mpv process spawning and JSON IPC protocol
- `protocol.rs`: Jellyfin API types and WebSocket message parsing
- `config.rs`: CLI args and environment variable handling
- `tray.rs`: Windows system tray icon and menu
- `login_ui.rs` / `settings_ui.rs`: Windows dialogs
- `credential_store.rs`: Windows Credential Manager for saved logins
- `skip_settings.rs`: Media segment skip preferences

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
