use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use crate::skip_settings::PlaybackMode;

trait AsyncIpc: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIpc for T {}
type IpcStream = Box<dyn AsyncIpc>;

const IPC_CONNECT_ATTEMPTS: usize = 500;
const IPC_CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(windows)]
fn bring_mpv_window_to_front(process_id: Option<u32>) {
    const WINDOW_DISCOVERY_RETRIES: usize = 10;
    const WINDOW_DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(100);

    let Some(process_id) = process_id else {
        return;
    };

    // `file-loaded` can arrive just before mpv maps its native window. Run the
    // short discovery retry outside the IPC actor so playback events continue
    // to be processed while the window becomes visible.
    std::mem::drop(tokio::task::spawn_blocking(move || {
        for attempt in 0..WINDOW_DISCOVERY_RETRIES {
            match window_activation::bring_to_front(process_id) {
                window_activation::ActivationResult::Activated => return,
                window_activation::ActivationResult::WindowNotFound
                    if attempt + 1 < WINDOW_DISCOVERY_RETRIES =>
                {
                    std::thread::sleep(WINDOW_DISCOVERY_RETRY_INTERVAL);
                }
                window_activation::ActivationResult::WindowNotFound => {
                    debug!(%process_id, "mpv 播放窗口未及时创建，无法置于前台");
                    return;
                }
                window_activation::ActivationResult::Rejected => {
                    debug!(%process_id, "Windows 拒绝将 mpv 播放窗口置于前台");
                    return;
                }
            }
        }
    }));
}

#[cfg(not(windows))]
const fn bring_mpv_window_to_front(_process_id: Option<u32>) {}

#[cfg(windows)]
mod window_activation {
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM},
        System::Threading::{AttachThreadInput, GetCurrentThreadId},
        UI::WindowsAndMessaging::{
            BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
            IsWindowVisible, MSG, PM_NOREMOVE, PeekMessageW, SW_RESTORE, SetForegroundWindow,
            ShowWindowAsync,
        },
    };

    #[derive(Debug)]
    pub(super) enum ActivationResult {
        Activated,
        WindowNotFound,
        Rejected,
    }

    struct WindowSearch {
        process_id: u32,
        hwnd: HWND,
    }

    pub(super) fn bring_to_front(process_id: u32) -> ActivationResult {
        let mut search = WindowSearch {
            process_id,
            hwnd: std::ptr::null_mut(),
        };
        unsafe {
            EnumWindows(Some(find_window_for_process), (&raw mut search) as LPARAM);
        }

        if search.hwnd.is_null() {
            return ActivationResult::WindowNotFound;
        }

        if unsafe { GetForegroundWindow() } == search.hwnd {
            return ActivationResult::Activated;
        }

        unsafe {
            if IsIconic(search.hwnd) != 0 {
                ShowWindowAsync(search.hwnd, SW_RESTORE);
            }

            // A WebSocket command is handled by a background process, which
            // normally has no permission to call SetForegroundWindow. Attach
            // only for this activation attempt to the foreground input queue;
            // this gives the request the same focus context without making mpv
            // permanently topmost.
            let foreground = GetForegroundWindow();
            let foreground_thread = if foreground.is_null() {
                0
            } else {
                GetWindowThreadProcessId(foreground, std::ptr::null_mut())
            };
            let current_thread = GetCurrentThreadId();
            let mut message = MSG::default();
            PeekMessageW(&raw mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
            let input_attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, 1) != 0;

            BringWindowToTop(search.hwnd);
            let activated =
                SetForegroundWindow(search.hwnd) != 0 || GetForegroundWindow() == search.hwnd;

            if input_attached {
                AttachThreadInput(current_thread, foreground_thread, 0);
            }

            if activated || GetForegroundWindow() == search.hwnd {
                ActivationResult::Activated
            } else {
                ActivationResult::Rejected
            }
        }
    }

    unsafe extern "system" fn find_window_for_process(hwnd: HWND, lparam: LPARAM) -> i32 {
        let search = unsafe { &mut *(lparam as *mut WindowSearch) };
        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &raw mut process_id) };
        if process_id != search.process_id || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        search.hwnd = hwnd;
        0
    }
}

#[derive(Debug)]
pub enum MpvEvent {
    FileLoaded {
        id: Uuid,
    },
    Position {
        id: Uuid,
        seconds: f64,
    },
    Paused {
        id: Uuid,
        paused: bool,
    },
    Buffering {
        id: Uuid,
        buffering: bool,
    },
    Muted {
        id: Uuid,
        muted: bool,
    },
    Volume {
        id: Uuid,
        volume: f64,
    },
    AudioTracks {
        id: Uuid,
        audio_track_ids: HashMap<i32, i32>,
    },
    Preview {
        id: Uuid,
        request: Option<MpvPreviewRequest>,
    },
    BitrateSelection {
        id: Uuid,
        selection: MpvBitrateSelection,
    },
    Ended {
        id: Uuid,
        failed: bool,
    },
    Terminated {
        id: Uuid,
        failed: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MpvBitrateSelection {
    #[default]
    Auto,
    Adaptive,
    Limit(u64),
}

impl MpvBitrateSelection {
    pub const fn max_streaming_bitrate(self) -> Option<u64> {
        match self {
            Self::Auto | Self::Adaptive => None,
            Self::Limit(value) => Some(value),
        }
    }

    pub const fn enables_auto_adjustment(self) -> bool {
        matches!(self, Self::Adaptive)
    }

    pub fn menu_token(self) -> String {
        match self {
            Self::Auto => "auto".to_owned(),
            Self::Adaptive => "adaptive".to_owned(),
            Self::Limit(value) => format!("bitrate-{value}"),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Auto => "自动（不限制）".to_owned(),
            Self::Adaptive => "自动调整码率".to_owned(),
            Self::Limit(value) => {
                let tenths = value / 100_000 + u64::from(value % 100_000 >= 50_000);
                format!("码率上限：{}.{:01} Mbps", tenths / 10, tenths % 10)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MpvPreviewRequest {
    pub seconds: f64,
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct Overlay {
    pub id: u32,
    pub x: i64,
    pub y: i64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
}

enum MpvCommand {
    Headers(Vec<String>),
    BitrateSelection(MpvBitrateSelection),
    LoadFile(LoadRequest),
    Pause(bool),
    Seek(f64),
    Volume(f64),
    Mute(bool),
    ToggleMute,
    ToggleFullscreen,
    PlaybackRate(f64),
    AudioTrack(Option<i32>),
    LoadSubtitle(String),
    DisableSubtitle,
    AddOverlay {
        overlay: Overlay,
        completed: oneshot::Sender<std::result::Result<(), String>>,
    },
    RemoveOverlay(u32),
    Quit,
}

struct LoadRequest {
    url: Url,
    start_seconds: f64,
    subtitle_url: Option<Url>,
}

#[derive(Clone)]
pub struct MpvHandle {
    pub id: Uuid,
    commands: mpsc::Sender<MpvCommand>,
}

impl MpvHandle {
    pub async fn set_http_headers(&self, headers: Vec<String>) -> Result<()> {
        self.send(MpvCommand::Headers(headers)).await
    }

    pub async fn set_bitrate_selection(&self, selection: MpvBitrateSelection) -> Result<()> {
        self.send(MpvCommand::BitrateSelection(selection)).await
    }

    pub async fn set_pause(&self, paused: bool) -> Result<()> {
        self.send(MpvCommand::Pause(paused)).await
    }

    pub async fn load_file(
        &self,
        url: Url,
        start_seconds: f64,
        subtitle_url: Option<Url>,
    ) -> Result<()> {
        self.send(MpvCommand::LoadFile(LoadRequest {
            url,
            start_seconds,
            subtitle_url,
        }))
        .await
    }

    pub async fn seek(&self, seconds: f64) -> Result<()> {
        self.send(MpvCommand::Seek(seconds)).await
    }

    pub async fn set_volume(&self, volume: f64) -> Result<()> {
        self.send(MpvCommand::Volume(volume)).await
    }

    pub async fn set_mute(&self, muted: bool) -> Result<()> {
        self.send(MpvCommand::Mute(muted)).await
    }

    pub async fn toggle_mute(&self) -> Result<()> {
        self.send(MpvCommand::ToggleMute).await
    }

    pub async fn toggle_fullscreen(&self) -> Result<()> {
        self.send(MpvCommand::ToggleFullscreen).await
    }

    pub async fn set_playback_rate(&self, rate: f64) -> Result<()> {
        self.send(MpvCommand::PlaybackRate(rate)).await
    }

    pub async fn set_audio_track(&self, track_id: Option<i32>) -> Result<()> {
        self.send(MpvCommand::AudioTrack(track_id)).await
    }

    pub async fn load_subtitle(&self, url: Url) -> Result<()> {
        self.send(MpvCommand::LoadSubtitle(url.into())).await
    }

    pub async fn disable_subtitle(&self) -> Result<()> {
        self.send(MpvCommand::DisableSubtitle).await
    }

    pub async fn add_overlay(&self, overlay: Overlay) -> Result<()> {
        let (completed, result) = oneshot::channel();
        self.send(MpvCommand::AddOverlay { overlay, completed })
            .await?;
        result
            .await
            .map_err(|_| anyhow!("mpv 缩略图覆盖层命令意外中断"))?
            .map_err(|message| anyhow!(message))
    }

    pub async fn remove_overlay(&self, id: u32) -> Result<()> {
        self.send(MpvCommand::RemoveOverlay(id)).await
    }

    pub async fn quit(&self) -> Result<()> {
        self.send(MpvCommand::Quit).await
    }

    async fn send(&self, command: MpvCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| anyhow!("mpv JSON IPC 已断开"))
    }
}

pub struct MpvLaunch {
    pub executable: PathBuf,
    pub extra_args: Vec<OsString>,
    pub window_title: String,
    pub bring_to_front_on_start: bool,
    pub playback_mode: PlaybackMode,
    pub url: Url,
    pub headers: Vec<String>,
    pub start_seconds: f64,
    pub subtitle_url: Option<Url>,
    pub thumbnail_size: Option<(u32, u32)>,
    pub bitrate_selection: MpvBitrateSelection,
    pub events: mpsc::Sender<MpvEvent>,
}

pub async fn spawn_mpv(launch: MpvLaunch) -> Result<MpvHandle> {
    let MpvLaunch {
        executable,
        extra_args,
        window_title,
        bring_to_front_on_start,
        playback_mode,
        url,
        headers,
        start_seconds,
        subtitle_url,
        mut thumbnail_size,
        bitrate_selection,
        events,
    } = launch;
    let id = Uuid::new_v4();
    let ipc_path = ipc_path(id);
    let window_state = effective_window_state(playback_mode, &extra_args);
    let script_directory = match prepare_script_directory(id).await {
        Ok(directory) => {
            if thumbnail_size.is_some()
                && let Err(error) = prepare_thumbnail_script(&directory).await
            {
                warn!(%error, "无法准备 mpv 缩略图桥接脚本，继续普通播放");
                thumbnail_size = None;
            }
            Some(directory)
        }
        Err(error) => {
            warn!(%error, "无法准备 mpv 码率菜单脚本，继续普通播放");
            thumbnail_size = None;
            None
        }
    };
    let mut command = Command::new(&executable);
    command.args(window_args(window_state, &extra_args));
    if let Some(directory) = script_directory.as_ref() {
        command.arg(format!(
            "--script={}",
            directory.join("quality_menu.lua").to_string_lossy()
        ));
    }
    if thumbnail_size.is_some()
        && let Some(directory) = script_directory.as_ref()
    {
        command.arg(format!(
            "--script={}",
            directory.join("thumbfast.lua").to_string_lossy()
        ));
    }
    command
        .arg("--idle=yes")
        .arg("--terminal=no")
        .arg("--input-terminal=no")
        .arg("--force-window=yes")
        .arg(format!("--input-ipc-server={}", ipc_path.to_string_lossy()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            cleanup_script_directory(script_directory.as_deref()).await;
            return Err(error).with_context(|| format!("无法启动 mpv: {}", executable.display()));
        }
    };
    let (command_tx, command_rx) = mpsc::channel(32);
    let (startup_tx, startup_rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut startup_tx = Some(startup_tx);
        let result = run_actor(
            id,
            child,
            ipc_path.clone(),
            url,
            headers,
            window_title,
            window_state,
            bring_to_front_on_start,
            start_seconds,
            subtitle_url,
            thumbnail_size,
            bitrate_selection,
            command_rx,
            events.clone(),
            &mut startup_tx,
        )
        .await;

        if let Err(error) = result {
            if let Some(startup_tx) = startup_tx.take() {
                let _ = startup_tx.send(Err(error.to_string()));
            } else {
                warn!(%error, "mpv IPC 任务异常退出");
                let _ = events.send(MpvEvent::Terminated { id, failed: true }).await;
            }
        }
        cleanup_ipc(&ipc_path).await;
        cleanup_script_directory(script_directory.as_deref()).await;
    });

    match timeout(Duration::from_secs(15), startup_rx).await {
        Ok(Ok(Ok(()))) => Ok(MpvHandle {
            id,
            commands: command_tx,
        }),
        Ok(Ok(Err(message))) => bail!("mpv 启动失败: {message}"),
        Ok(Err(_)) => bail!("mpv 启动任务意外退出"),
        Err(_) => bail!("等待 mpv JSON IPC 超时"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowState {
    maximized: bool,
    fullscreen: bool,
}

impl WindowState {
    const fn from_playback_settings(playback_mode: PlaybackMode) -> Self {
        match playback_mode {
            PlaybackMode::Windowed => Self {
                maximized: false,
                fullscreen: false,
            },
            PlaybackMode::Maximized => Self {
                maximized: true,
                fullscreen: false,
            },
            PlaybackMode::Fullscreen => Self {
                maximized: false,
                fullscreen: true,
            },
        }
    }
}

fn effective_window_state(playback_mode: PlaybackMode, extra_args: &[OsString]) -> WindowState {
    let mut state = WindowState::from_playback_settings(playback_mode);
    for argument in extra_args {
        match argument.to_string_lossy().as_ref() {
            "--window-maximized" | "--window-maximized=yes" | "--window-maximized=true" => {
                state.maximized = true;
            }
            "--window-maximized=no" | "--window-maximized=false" | "--no-window-maximized" => {
                state.maximized = false;
            }
            "--fullscreen" | "--fullscreen=yes" | "--fullscreen=true" => {
                state.fullscreen = true;
            }
            "--fullscreen=no" | "--fullscreen=false" | "--no-fullscreen" => {
                state.fullscreen = false;
            }
            _ => {}
        }
    }
    state
}

fn window_args(window_state: WindowState, extra_args: &[OsString]) -> Vec<OsString> {
    let mut args = vec![
        mpv_boolean_option("--window-maximized", window_state.maximized),
        mpv_boolean_option("--fullscreen", window_state.fullscreen),
    ];
    args.extend_from_slice(extra_args);
    args
}

fn mpv_boolean_option(name: &str, value: bool) -> OsString {
    OsString::from(format!("{name}={}", if value { "yes" } else { "no" }))
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    id: Uuid,
    mut child: Child,
    ipc_path: PathBuf,
    url: Url,
    headers: Vec<String>,
    window_title: String,
    window_state: WindowState,
    bring_to_front_on_start: bool,
    start_seconds: f64,
    subtitle_url: Option<Url>,
    thumbnail_size: Option<(u32, u32)>,
    initial_bitrate_selection: MpvBitrateSelection,
    mut commands: mpsc::Receiver<MpvCommand>,
    events: mpsc::Sender<MpvEvent>,
    startup: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    let stream = connect_ipc(&ipc_path, &mut child).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let mut next_request_id = 10_000_u64;
    let mut pending_overlay_commands = HashMap::new();
    let mut pending_load = Some(LoadRequest {
        url,
        start_seconds,
        subtitle_url,
    });
    let mut needs_initial_activation = bring_to_front_on_start;
    let mut bitrate_selection = initial_bitrate_selection;

    send_json(
        &mut writer,
        json!({"command": ["set_property", "http-header-fields", headers]}),
    )
    .await?;
    set_window_state(&mut writer, &window_title, window_state).await?;
    for (number, property) in [
        "time-pos",
        "pause",
        "paused-for-cache",
        "mute",
        "volume",
        "track-list",
    ]
    .iter()
    .enumerate()
    {
        send_json(
            &mut writer,
            json!({"command": ["observe_property", number + 1, property]}),
        )
        .await?;
    }
    send_load_file(
        &mut writer,
        pending_load.as_ref().expect("初始加载请求已准备"),
    )
    .await?;
    if let Some(startup) = startup.take() {
        let _ = startup.send(Ok(()));
    }

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    let _ = send_json(&mut writer, json!({"command": ["quit"]})).await;
                    break;
                };
                match command {
                    MpvCommand::BitrateSelection(selection) => {
                        bitrate_selection = selection;
                        configure_bitrate_menu(&mut writer, bitrate_selection).await?;
                    }
                    MpvCommand::LoadFile(load) => {
                        pending_load = Some(load);
                        send_load_file(
                            &mut writer,
                            pending_load.as_ref().expect("待加载请求已准备"),
                        )
                        .await?;
                    }
                    MpvCommand::AddOverlay { overlay, completed } => {
                        let request_id = next_request_id;
                        next_request_id = next_request_id.wrapping_add(1);
                        send_json(&mut writer, overlay_command(&overlay, request_id)).await?;
                        pending_overlay_commands.insert(request_id, completed);
                    }
                    MpvCommand::RemoveOverlay(id) => {
                        send_json(&mut writer, json!({"command": ["overlay-remove", id]})).await?;
                    }
                    MpvCommand::Quit => {
                        send_mpv_command(&mut writer, MpvCommand::Quit).await?;
                        break;
                    }
                    command => send_mpv_command(&mut writer, command).await?,
                }
            }
            line = lines.next_line() => {
                match line.context("读取 mpv JSON IPC 失败")? {
                    Some(line) => {
                        let value: Value = match serde_json::from_str(&line) {
                            Ok(value) => value,
                            Err(error) => {
                                debug!(%error, "忽略无法解析的 mpv IPC 消息");
                                continue;
                            }
                        };
                        if complete_overlay_command(&value, &mut pending_overlay_commands) {
                            continue;
                        }
                        if value.get("event").and_then(Value::as_str) == Some("file-loaded") {
                            set_window_state(&mut writer, &window_title, window_state).await?;
                            if needs_initial_activation {
                                bring_mpv_window_to_front(child.id());
                                needs_initial_activation = false;
                            }
                            if let Some(load) = pending_load.take() {
                                if load.start_seconds > 0.0 {
                                    send_json(
                                        &mut writer,
                                        json!({"command": ["seek", load.start_seconds, "absolute", "exact"]}),
                                    ).await?;
                                }
                                if let Some(subtitle_url) = load.subtitle_url.as_ref() {
                                    send_json(
                                        &mut writer,
                                        json!({"command": ["sub-add", subtitle_url.as_str(), "select"]}),
                                    ).await?;
                                }
                            }
                            if let Some((width, height)) = thumbnail_size {
                                configure_thumbnail_script(&mut writer, width, height).await?;
                            }
                            configure_bitrate_menu(&mut writer, bitrate_selection).await?;
                            let _ = events.send(MpvEvent::FileLoaded { id }).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("property-change") {
                            emit_property(id, &value, &events).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("client-message") {
                            emit_client_message(id, &value, &events).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("end-file") {
                            let failed = value.get("reason").and_then(Value::as_str) == Some("error");
                            if failed || pending_load.is_none() {
                                let _ = events.send(MpvEvent::Ended { id, failed }).await;
                            }
                        } else if value.get("event").and_then(Value::as_str) == Some("shutdown") {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    for (_, completed) in pending_overlay_commands.drain() {
        let _ = completed.send(Err("mpv 已退出".to_owned()));
    }

    match timeout(Duration::from_secs(3), child.wait()).await {
        Ok(result) => {
            result.context("等待 mpv 退出失败")?;
        }
        Err(_) => {
            child.kill().await.context("终止无响应的 mpv 失败")?;
        }
    }
    let _ = events
        .send(MpvEvent::Terminated { id, failed: false })
        .await;
    Ok(())
}

fn overlay_command(overlay: &Overlay, request_id: u64) -> Value {
    let stride = overlay.width.saturating_mul(4);
    json!({
        "command": [
            "overlay-add",
            overlay.id,
            overlay.x,
            overlay.y,
            overlay.path.to_string_lossy(),
            0,
            "bgra",
            overlay.width,
            overlay.height,
            stride,
            overlay.display_width,
            overlay.display_height
        ],
        "request_id": request_id
    })
}

fn complete_overlay_command(
    value: &Value,
    pending: &mut HashMap<u64, oneshot::Sender<std::result::Result<(), String>>>,
) -> bool {
    let Some(request_id) = value.get("request_id").and_then(Value::as_u64) else {
        return false;
    };
    let Some(completed) = pending.remove(&request_id) else {
        return false;
    };
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("未知 mpv IPC 错误");
    let result = if error == "success" {
        Ok(())
    } else {
        Err(format!("mpv overlay-add 失败: {error}"))
    };
    let _ = completed.send(result);
    true
}

async fn configure_thumbnail_script<W: AsyncWrite + Unpin>(
    writer: &mut W,
    width: u32,
    height: u32,
) -> Result<()> {
    send_json(
        writer,
        json!({"command": [
            "script-message-to",
            "thumbfast",
            "jellyfin-mpv-thumbnail-info",
            width.to_string(),
            height.to_string()
        ]}),
    )
    .await
}

async fn configure_bitrate_menu<W: AsyncWrite + Unpin>(
    writer: &mut W,
    selection: MpvBitrateSelection,
) -> Result<()> {
    send_json(
        writer,
        json!({"command": [
            "script-message",
            "jellyfin-mpv-bitrate-selected",
            selection.menu_token(),
            selection.label()
        ]}),
    )
    .await
}

async fn send_load_file<W: AsyncWrite + Unpin>(writer: &mut W, load: &LoadRequest) -> Result<()> {
    send_json(
        writer,
        json!({"command": ["loadfile", load.url.as_str(), "replace"]}),
    )
    .await
}

async fn set_window_state<W: AsyncWrite + Unpin>(
    writer: &mut W,
    title: &str,
    window_state: WindowState,
) -> Result<()> {
    send_json(
        writer,
        json!({"command": ["set_property", "force-media-title", title]}),
    )
    .await?;
    send_json(writer, json!({"command": ["set_property", "title", title]})).await?;
    send_json(
        writer,
        json!({"command": ["set_property", "window-maximized", window_state.maximized]}),
    )
    .await?;
    send_json(
        writer,
        json!({"command": ["set_property", "fullscreen", window_state.fullscreen]}),
    )
    .await
}

async fn send_mpv_command<W: AsyncWrite + Unpin>(
    writer: &mut W,
    command: MpvCommand,
) -> Result<()> {
    let value = match command {
        MpvCommand::Headers(headers) => {
            json!({"command": ["set_property", "http-header-fields", headers]})
        }
        MpvCommand::Pause(paused) => json!({"command": ["set_property", "pause", paused]}),
        MpvCommand::Seek(seconds) => {
            json!({"command": ["seek", seconds.max(0.0), "absolute", "exact"]})
        }
        MpvCommand::Volume(volume) => {
            json!({"command": ["set_property", "volume", volume.clamp(0.0, 100.0)]})
        }
        MpvCommand::Mute(muted) => json!({"command": ["set_property", "mute", muted]}),
        MpvCommand::ToggleMute => json!({"command": ["cycle", "mute"]}),
        MpvCommand::ToggleFullscreen => json!({"command": ["cycle", "fullscreen"]}),
        MpvCommand::PlaybackRate(rate) => {
            json!({"command": ["set_property", "speed", rate.max(0.01)]})
        }
        MpvCommand::AudioTrack(track_id) => {
            let track_id = track_id.map_or_else(|| Value::String("no".to_owned()), Value::from);
            json!({"command": ["set_property", "aid", track_id]})
        }
        MpvCommand::LoadSubtitle(url) => {
            send_json(writer, json!({"command": ["sub-remove"]})).await?;
            json!({"command": ["sub-add", url, "select"]})
        }
        MpvCommand::DisableSubtitle => json!({"command": ["set_property", "sid", "no"]}),
        MpvCommand::BitrateSelection(_)
        | MpvCommand::LoadFile(_)
        | MpvCommand::AddOverlay { .. }
        | MpvCommand::RemoveOverlay(_) => {
            unreachable!("码率菜单和覆盖层命令由 mpv actor 单独处理")
        }
        MpvCommand::Quit => json!({"command": ["quit"]}),
    };
    send_json(writer, value).await
}

async fn send_json<W: AsyncWrite + Unpin>(writer: &mut W, value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value).context("序列化 mpv IPC 命令失败")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("写入 mpv JSON IPC 失败")
}

async fn emit_property(id: Uuid, value: &Value, events: &mpsc::Sender<MpvEvent>) {
    let property = value.get("name").and_then(Value::as_str);
    let data = value.get("data").unwrap_or(&Value::Null);
    let event = match property {
        Some("time-pos") => data
            .as_f64()
            .map(|seconds| MpvEvent::Position { id, seconds }),
        Some("pause") => data.as_bool().map(|paused| MpvEvent::Paused { id, paused }),
        Some("paused-for-cache") => data
            .as_bool()
            .map(|buffering| MpvEvent::Buffering { id, buffering }),
        Some("mute") => data.as_bool().map(|muted| MpvEvent::Muted { id, muted }),
        Some("volume") => data.as_f64().map(|volume| MpvEvent::Volume { id, volume }),
        Some("track-list") => {
            parse_audio_track_ids(data).map(|audio_track_ids| MpvEvent::AudioTracks {
                id,
                audio_track_ids,
            })
        }
        _ => None,
    };
    if let Some(event) = event {
        let _ = events.send(event).await;
    }
}

fn parse_audio_track_ids(data: &Value) -> Option<HashMap<i32, i32>> {
    let tracks = data.as_array()?;
    Some(
        tracks
            .iter()
            .filter_map(|track| {
                if track.get("type").and_then(Value::as_str) != Some("audio") {
                    return None;
                }
                let ff_index = track
                    .get("ff-index")
                    .and_then(Value::as_i64)
                    .and_then(|value| i32::try_from(value).ok())?;
                let track_id = track
                    .get("id")
                    .and_then(Value::as_i64)
                    .and_then(|value| i32::try_from(value).ok())?;
                Some((ff_index, track_id))
            })
            .collect(),
    )
}

async fn emit_client_message(id: Uuid, value: &Value, events: &mpsc::Sender<MpvEvent>) {
    let Some(arguments) = value.get("args").and_then(Value::as_array) else {
        return;
    };
    match arguments.first().and_then(Value::as_str) {
        Some("jellyfin-mpv-preview") if arguments.len() >= 6 => {
            let Some(seconds) = value_as_f64(&arguments[1]) else {
                return;
            };
            let Some(x) = value_as_f64(&arguments[2]) else {
                return;
            };
            let Some(y) = value_as_f64(&arguments[3]) else {
                return;
            };
            let width = nonnegative_f64_to_u32(value_as_f64(&arguments[4]).unwrap_or(0.0));
            let height = nonnegative_f64_to_u32(value_as_f64(&arguments[5]).unwrap_or(0.0));
            let request = MpvPreviewRequest {
                seconds,
                x: rounded_f64_to_i64(x),
                y: rounded_f64_to_i64(y),
                width,
                height,
            };
            let _ = events.try_send(MpvEvent::Preview {
                id,
                request: Some(request),
            });
        }
        Some("jellyfin-mpv-preview-clear") => {
            let _ = events.send(MpvEvent::Preview { id, request: None }).await;
        }
        Some("jellyfin-mpv-bitrate") if arguments.len() >= 2 => {
            let Some(selection) = arguments[1].as_str().and_then(parse_bitrate_selection) else {
                return;
            };
            let _ = events
                .send(MpvEvent::BitrateSelection { id, selection })
                .await;
        }
        _ => {}
    }
}

fn parse_bitrate_selection(value: &str) -> Option<MpvBitrateSelection> {
    match value {
        "auto" => return Some(MpvBitrateSelection::Auto),
        "adaptive" => return Some(MpvBitrateSelection::Adaptive),
        _ => {}
    }
    let value = value.strip_prefix("bitrate-")?.parse::<u64>().ok()?;
    (value > 0 && i32::try_from(value).is_ok()).then_some(MpvBitrateSelection::Limit(value))
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|number| number.is_finite())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn nonnegative_f64_to_u32(value: f64) -> u32 {
    value.max(0.0) as u32
}

#[allow(clippy::cast_possible_truncation)]
const fn rounded_f64_to_i64(value: f64) -> i64 {
    value.round() as i64
}

async fn prepare_script_directory(id: Uuid) -> Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!("jellyfin-mpv-script-{id}"));
    tokio::fs::create_dir(&directory)
        .await
        .with_context(|| format!("创建 mpv 脚本临时目录失败: {}", directory.display()))?;
    if let Err(error) = tokio::fs::write(
        directory.join("quality_menu.lua"),
        include_bytes!("quality_menu.lua"),
    )
    .await
    {
        let _ = tokio::fs::remove_dir_all(&directory).await;
        return Err(error).context("写入 mpv 码率菜单脚本失败");
    }
    Ok(directory)
}

async fn prepare_thumbnail_script(directory: &Path) -> Result<()> {
    tokio::fs::write(
        directory.join("thumbfast.lua"),
        include_bytes!("thumbfast.lua"),
    )
    .await
    .context("写入 mpv 缩略图桥接脚本失败")
}

async fn cleanup_script_directory(directory: Option<&Path>) {
    let Some(directory) = directory else {
        return;
    };
    if let Err(error) = tokio::fs::remove_dir_all(directory).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        debug!(%error, path = %directory.display(), "清理 mpv 脚本临时目录失败");
    }
}

fn ipc_path(id: Uuid) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\jellyfin-mpv-{id}"))
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir().join(format!("jellyfin-mpv-{id}.sock"))
    }
}

#[cfg(windows)]
async fn connect_ipc(path: &Path, child: &mut Child) -> Result<IpcStream> {
    use tokio::net::windows::named_pipe::ClientOptions;

    for _ in 0..IPC_CONNECT_ATTEMPTS {
        if let Some(status) = child.try_wait().context("检查 mpv 状态失败")? {
            bail!("mpv 在 IPC 就绪前退出，状态: {status}");
        }
        match ClientOptions::new().open(path) {
            Ok(pipe) => return Ok(Box::new(pipe)),
            Err(_) => sleep(IPC_CONNECT_RETRY_INTERVAL).await,
        }
    }
    bail!("无法连接 mpv 命名管道 {}", path.display())
}

#[cfg(unix)]
async fn connect_ipc(path: &Path, child: &mut Child) -> Result<IpcStream> {
    use tokio::net::UnixStream;

    for _ in 0..IPC_CONNECT_ATTEMPTS {
        if let Some(status) = child.try_wait().context("检查 mpv 状态失败")? {
            bail!("mpv 在 IPC 就绪前退出，状态: {status}");
        }
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(Box::new(stream)),
            Err(_) => sleep(IPC_CONNECT_RETRY_INTERVAL).await,
        }
    }
    bail!("无法连接 mpv Unix socket {}", path.display())
}

#[cfg_attr(windows, allow(clippy::unused_async))]
async fn cleanup_ipc(path: &Path) {
    #[cfg(unix)]
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        debug!(%error, path = %path.display(), "清理 mpv IPC socket 失败");
    }

    #[cfg(windows)]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, duplex};

    use super::*;

    #[test]
    fn applies_playback_preferences_and_allows_mpv_argument_overrides() {
        assert_eq!(
            WindowState::from_playback_settings(PlaybackMode::Windowed),
            WindowState {
                maximized: false,
                fullscreen: false,
            }
        );
        assert_eq!(
            WindowState::from_playback_settings(PlaybackMode::Maximized),
            WindowState {
                maximized: true,
                fullscreen: false,
            }
        );
        assert_eq!(
            WindowState::from_playback_settings(PlaybackMode::Fullscreen),
            WindowState {
                maximized: false,
                fullscreen: true,
            }
        );

        let extra_args = [
            OsString::from("--no-fullscreen"),
            OsString::from("--window-maximized=yes"),
        ];
        let state = effective_window_state(PlaybackMode::Fullscreen, &extra_args);
        assert_eq!(
            state,
            WindowState {
                maximized: true,
                fullscreen: false,
            }
        );

        let args = window_args(state, &extra_args);
        assert_eq!(args[0], "--window-maximized=yes");
        assert_eq!(args[1], "--fullscreen=no");
        assert_eq!(args[2], "--no-fullscreen");
    }

    #[tokio::test]
    async fn sets_media_and_window_titles_through_ipc() {
        let (mut writer, mut reader) = duplex(1024);
        set_window_state(
            &mut writer,
            "剧集名称 S01E02",
            WindowState {
                maximized: false,
                fullscreen: true,
            },
        )
        .await
        .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let messages: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            messages[0]["command"],
            json!(["set_property", "force-media-title", "剧集名称 S01E02"])
        );
        assert_eq!(
            messages[1]["command"],
            json!(["set_property", "title", "剧集名称 S01E02"])
        );
        assert_eq!(
            messages[2]["command"],
            json!(["set_property", "window-maximized", false])
        );
        assert_eq!(
            messages[3]["command"],
            json!(["set_property", "fullscreen", true])
        );
    }

    #[tokio::test]
    async fn emits_http_header_update_command() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(
            &mut writer,
            MpvCommand::Headers(vec!["X-Emby-Token: new-token".to_owned()]),
        )
        .await
        .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            message["command"],
            json!([
                "set_property",
                "http-header-fields",
                ["X-Emby-Token: new-token"]
            ])
        );
    }

    #[tokio::test]
    async fn emits_subtitle_load_and_disable_commands() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(
            &mut writer,
            MpvCommand::LoadSubtitle("http://example/subtitle.srt".to_owned()),
        )
        .await
        .unwrap();
        send_mpv_command(&mut writer, MpvCommand::DisableSubtitle)
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let messages: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["command"][0], "sub-remove");
        assert_eq!(messages[1]["command"][0], "sub-add");
        assert_eq!(messages[1]["command"][1], "http://example/subtitle.srt");
        assert_eq!(messages[2]["command"][1], "sid");
        assert_eq!(messages[2]["command"][2], "no");
    }

    #[tokio::test]
    async fn emits_audio_track_selection_and_disable_commands() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(&mut writer, MpvCommand::AudioTrack(Some(3)))
            .await
            .unwrap();
        send_mpv_command(&mut writer, MpvCommand::AudioTrack(None))
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let messages: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages[0]["command"], json!(["set_property", "aid", 3]));
        assert_eq!(messages[1]["command"], json!(["set_property", "aid", "no"]));
    }

    #[tokio::test]
    async fn emits_fullscreen_toggle_command() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(&mut writer, MpvCommand::ToggleFullscreen)
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(message["command"], json!(["cycle", "fullscreen"]));
    }

    #[tokio::test]
    async fn emits_playback_rate_command() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(&mut writer, MpvCommand::PlaybackRate(1.25))
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(message["command"], json!(["set_property", "speed", 1.25]));
    }

    #[tokio::test]
    async fn parses_audio_track_mapping_from_track_list() {
        let (events, mut receiver) = mpsc::channel(1);
        let id = Uuid::new_v4();
        emit_property(
            id,
            &json!({
                "name": "track-list",
                "data": [
                    {"type": "video", "id": 1, "ff-index": 0},
                    {"type": "audio", "id": 2, "ff-index": 1},
                    {"type": "audio", "id": 4, "ff-index": 3}
                ]
            }),
            &events,
        )
        .await;

        let MpvEvent::AudioTracks {
            id: event_id,
            audio_track_ids,
        } = receiver.recv().await.expect("应产生音轨映射事件")
        else {
            panic!("事件类型错误");
        };
        assert_eq!(event_id, id);
        assert_eq!(audio_track_ids.get(&1), Some(&2));
        assert_eq!(audio_track_ids.get(&3), Some(&4));
        assert!(!audio_track_ids.contains_key(&0));
    }

    #[tokio::test]
    async fn parses_cache_buffering_property() {
        let (events, mut receiver) = mpsc::channel(1);
        let id = Uuid::new_v4();
        emit_property(
            id,
            &json!({"name": "paused-for-cache", "data": true}),
            &events,
        )
        .await;

        assert!(matches!(
            receiver.recv().await,
            Some(MpvEvent::Buffering {
                id: event_id,
                buffering: true
            }) if event_id == id
        ));
    }

    #[tokio::test]
    async fn emits_replace_load_file_command() {
        let (mut writer, mut reader) = duplex(1024);
        let load = LoadRequest {
            url: Url::parse("https://example.test/video.m3u8").unwrap(),
            start_seconds: 12.5,
            subtitle_url: None,
        };
        send_load_file(&mut writer, &load).await.unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            message["command"],
            json!(["loadfile", "https://example.test/video.m3u8", "replace"])
        );
    }

    #[tokio::test]
    async fn emits_pause_and_resume_commands() {
        let (mut writer, mut reader) = duplex(1024);
        send_mpv_command(&mut writer, MpvCommand::Pause(true))
            .await
            .unwrap();
        send_mpv_command(&mut writer, MpvCommand::Pause(false))
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let messages: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            messages[0]["command"],
            json!(["set_property", "pause", true])
        );
        assert_eq!(
            messages[1]["command"],
            json!(["set_property", "pause", false])
        );
    }

    #[tokio::test]
    async fn parses_thumbnail_bridge_client_messages() {
        let (events, mut receiver) = mpsc::channel(4);
        let id = Uuid::new_v4();
        emit_client_message(
            id,
            &json!({
                "event": "client-message",
                "args": ["jellyfin-mpv-preview", "12.5", "100.4", "200.6", "320", "180"]
            }),
            &events,
        )
        .await;

        let MpvEvent::Preview {
            request: Some(request),
            ..
        } = receiver.recv().await.unwrap()
        else {
            panic!("未生成缩略图请求");
        };
        assert_eq!(
            request,
            MpvPreviewRequest {
                seconds: 12.5,
                x: 100,
                y: 201,
                width: 320,
                height: 180,
            }
        );
    }

    #[tokio::test]
    async fn parses_bitrate_menu_client_messages() {
        let (events, mut receiver) = mpsc::channel(1);
        let id = Uuid::new_v4();
        emit_client_message(
            id,
            &json!({
                "event": "client-message",
                "args": ["jellyfin-mpv-bitrate", "bitrate-8000000"]
            }),
            &events,
        )
        .await;

        assert!(matches!(
            receiver.recv().await,
            Some(MpvEvent::BitrateSelection {
                id: event_id,
                selection: MpvBitrateSelection::Limit(8_000_000),
            }) if event_id == id
        ));
    }

    #[test]
    fn accepts_only_valid_bitrate_menu_selections() {
        assert_eq!(
            parse_bitrate_selection("auto"),
            Some(MpvBitrateSelection::Auto)
        );
        assert_eq!(
            parse_bitrate_selection("adaptive"),
            Some(MpvBitrateSelection::Adaptive)
        );
        assert_eq!(
            parse_bitrate_selection("bitrate-8000000"),
            Some(MpvBitrateSelection::Limit(8_000_000))
        );
        assert_eq!(parse_bitrate_selection("bitrate-0"), None);
        assert_eq!(parse_bitrate_selection("bitrate-2147483648"), None);
        assert_eq!(parse_bitrate_selection("resolution-1920x1080"), None);
    }

    #[test]
    fn embedded_bitrate_menu_uses_f9() {
        let script = std::str::from_utf8(include_bytes!("quality_menu.lua")).unwrap();
        assert!(script.contains("\"F9\""));
        assert!(script.contains("jellyfin-mpv-bitrate"));
        assert!(script.contains("adaptive"));
        assert!(!script.contains("resolution-"));
    }

    #[tokio::test]
    async fn configures_bitrate_menu_with_current_selection() {
        let (mut writer, mut reader) = duplex(1024);
        configure_bitrate_menu(&mut writer, MpvBitrateSelection::Adaptive)
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            message["command"],
            json!([
                "script-message",
                "jellyfin-mpv-bitrate-selected",
                "adaptive",
                "自动调整码率"
            ])
        );
    }

    #[tokio::test]
    async fn completes_overlay_command_from_mpv_response() {
        let (completed, result) = oneshot::channel();
        let mut pending = HashMap::from([(42, completed)]);
        assert!(complete_overlay_command(
            &json!({"request_id": 42, "error": "success"}),
            &mut pending,
        ));
        assert!(result.await.unwrap().is_ok());
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn configures_thumbnail_script_with_string_arguments() {
        let (mut writer, mut reader) = duplex(1024);
        configure_thumbnail_script(&mut writer, 320, 180)
            .await
            .unwrap();
        drop(writer);

        let mut output = String::new();
        reader.read_to_string(&mut output).await.unwrap();
        let message: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            message["command"],
            json!([
                "script-message-to",
                "thumbfast",
                "jellyfin-mpv-thumbnail-info",
                "320",
                "180"
            ])
        );
    }
}
