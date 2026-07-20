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

trait AsyncIpc: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIpc for T {}
type IpcStream = Box<dyn AsyncIpc>;

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
    Muted {
        id: Uuid,
        muted: bool,
    },
    Volume {
        id: Uuid,
        volume: f64,
    },
    Preview {
        id: Uuid,
        request: Option<MpvPreviewRequest>,
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
    Pause(bool),
    Seek(f64),
    Volume(f64),
    Mute(bool),
    ToggleMute,
    LoadSubtitle(String),
    DisableSubtitle,
    AddOverlay {
        overlay: Overlay,
        completed: oneshot::Sender<std::result::Result<(), String>>,
    },
    RemoveOverlay(u32),
    Quit,
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

    pub async fn set_pause(&self, paused: bool) -> Result<()> {
        self.send(MpvCommand::Pause(paused)).await
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
    pub url: Url,
    pub headers: Vec<String>,
    pub start_seconds: f64,
    pub subtitle_url: Option<Url>,
    pub font_directory: Option<PathBuf>,
    pub thumbnail_size: Option<(u32, u32)>,
    pub events: mpsc::Sender<MpvEvent>,
}

pub async fn spawn_mpv(launch: MpvLaunch) -> Result<MpvHandle> {
    let MpvLaunch {
        executable,
        extra_args,
        window_title,
        url,
        headers,
        start_seconds,
        subtitle_url,
        font_directory,
        mut thumbnail_size,
        events,
    } = launch;
    let id = Uuid::new_v4();
    let ipc_path = ipc_path(id);
    let window_maximized = window_maximized(&extra_args);
    let thumbnail_script_directory = if thumbnail_size.is_some() {
        match prepare_thumbnail_script(id).await {
            Ok(directory) => Some(directory),
            Err(error) => {
                warn!(%error, "无法准备 mpv 缩略图桥接脚本，继续普通播放");
                thumbnail_size = None;
                None
            }
        }
    } else {
        None
    };
    let mut command = Command::new(&executable);
    command.args(window_args(&extra_args));
    if let Some(directory) = font_directory {
        command.arg(format!("--sub-fonts-dir={}", directory.to_string_lossy()));
    }
    if let Some(directory) = thumbnail_script_directory.as_ref() {
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
            cleanup_thumbnail_script_directory(thumbnail_script_directory.as_deref()).await;
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
            window_maximized,
            start_seconds,
            subtitle_url,
            thumbnail_size,
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
        cleanup_thumbnail_script_directory(thumbnail_script_directory.as_deref()).await;
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

fn window_args(extra_args: &[OsString]) -> Vec<OsString> {
    let mut args = vec![OsString::from("--window-maximized=yes")];
    args.extend_from_slice(extra_args);
    args
}

fn window_maximized(extra_args: &[OsString]) -> bool {
    let mut maximized = true;
    for argument in extra_args {
        match argument.to_string_lossy().as_ref() {
            "--window-maximized=yes" => maximized = true,
            "--window-maximized=no" | "--no-window-maximized" => maximized = false,
            _ => {}
        }
    }
    maximized
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    id: Uuid,
    mut child: Child,
    ipc_path: PathBuf,
    url: Url,
    headers: Vec<String>,
    window_title: String,
    window_maximized: bool,
    start_seconds: f64,
    subtitle_url: Option<Url>,
    thumbnail_size: Option<(u32, u32)>,
    mut commands: mpsc::Receiver<MpvCommand>,
    events: mpsc::Sender<MpvEvent>,
    startup: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    let stream = connect_ipc(&ipc_path, &mut child).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let mut next_request_id = 10_000_u64;
    let mut pending_overlay_commands = HashMap::new();

    send_json(
        &mut writer,
        json!({"command": ["set_property", "http-header-fields", headers]}),
    )
    .await?;
    set_window_state(&mut writer, &window_title, window_maximized).await?;
    for (number, property) in ["time-pos", "pause", "mute", "volume"].iter().enumerate() {
        send_json(
            &mut writer,
            json!({"command": ["observe_property", number + 1, property]}),
        )
        .await?;
    }
    send_json(
        &mut writer,
        json!({"command": ["loadfile", url.as_str(), "replace"]}),
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
                    MpvCommand::AddOverlay { overlay, completed } => {
                        let request_id = next_request_id;
                        next_request_id = next_request_id.wrapping_add(1);
                        send_json(&mut writer, overlay_command(overlay, request_id)).await?;
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
                            set_window_state(&mut writer, &window_title, window_maximized).await?;
                            if start_seconds > 0.0 {
                                send_json(
                                    &mut writer,
                                    json!({"command": ["seek", start_seconds, "absolute", "exact"]}),
                                ).await?;
                            }
                            if let Some(subtitle_url) = subtitle_url.as_ref() {
                                send_json(
                                    &mut writer,
                                    json!({"command": ["sub-add", subtitle_url.as_str(), "select"]}),
                                ).await?;
                            }
                            if let Some((width, height)) = thumbnail_size {
                                configure_thumbnail_script(&mut writer, width, height).await?;
                            }
                            let _ = events.send(MpvEvent::FileLoaded { id }).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("property-change") {
                            emit_property(id, &value, &events).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("client-message") {
                            emit_client_message(id, &value, &events).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("end-file") {
                            let failed = value.get("reason").and_then(Value::as_str) == Some("error");
                            let _ = events.send(MpvEvent::Ended { id, failed }).await;
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

fn overlay_command(overlay: Overlay, request_id: u64) -> Value {
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

async fn set_window_state<W: AsyncWrite + Unpin>(
    writer: &mut W,
    title: &str,
    maximized: bool,
) -> Result<()> {
    send_json(
        writer,
        json!({"command": ["set_property", "force-media-title", title]}),
    )
    .await?;
    send_json(writer, json!({"command": ["set_property", "title", title]})).await?;
    send_json(
        writer,
        json!({"command": ["set_property", "window-maximized", maximized]}),
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
        MpvCommand::LoadSubtitle(url) => {
            send_json(writer, json!({"command": ["sub-remove"]})).await?;
            json!({"command": ["sub-add", url, "select"]})
        }
        MpvCommand::DisableSubtitle => json!({"command": ["set_property", "sid", "no"]}),
        MpvCommand::AddOverlay { .. } | MpvCommand::RemoveOverlay(_) => {
            unreachable!("覆盖层命令由 mpv actor 单独处理")
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
        Some("mute") => data.as_bool().map(|muted| MpvEvent::Muted { id, muted }),
        Some("volume") => data.as_f64().map(|volume| MpvEvent::Volume { id, volume }),
        _ => None,
    };
    if let Some(event) = event {
        let _ = events.send(event).await;
    }
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
            let width = value_as_f64(&arguments[4]).unwrap_or(0.0).max(0.0) as u32;
            let height = value_as_f64(&arguments[5]).unwrap_or(0.0).max(0.0) as u32;
            let request = MpvPreviewRequest {
                seconds,
                x: x.round() as i64,
                y: y.round() as i64,
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
        _ => {}
    }
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|number| number.is_finite())
}

async fn prepare_thumbnail_script(id: Uuid) -> Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!("jellyfin-mpv-script-{id}"));
    tokio::fs::create_dir(&directory)
        .await
        .with_context(|| format!("创建 mpv 脚本临时目录失败: {}", directory.display()))?;
    if let Err(error) = tokio::fs::write(
        directory.join("thumbfast.lua"),
        include_bytes!("thumbfast.lua"),
    )
    .await
    {
        let _ = tokio::fs::remove_dir_all(&directory).await;
        return Err(error).context("写入 mpv 缩略图桥接脚本失败");
    }
    Ok(directory)
}

async fn cleanup_thumbnail_script_directory(directory: Option<&Path>) {
    let Some(directory) = directory else {
        return;
    };
    if let Err(error) = tokio::fs::remove_dir_all(directory).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        debug!(%error, path = %directory.display(), "清理 mpv 缩略图桥接脚本失败");
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

    for _ in 0..100 {
        if let Some(status) = child.try_wait().context("检查 mpv 状态失败")? {
            bail!("mpv 在 IPC 就绪前退出，状态: {status}");
        }
        match ClientOptions::new().open(path) {
            Ok(pipe) => return Ok(Box::new(pipe)),
            Err(_) => sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("无法连接 mpv 命名管道 {}", path.display())
}

#[cfg(unix)]
async fn connect_ipc(path: &Path, child: &mut Child) -> Result<IpcStream> {
    use tokio::net::UnixStream;

    for _ in 0..100 {
        if let Some(status) = child.try_wait().context("检查 mpv 状态失败")? {
            bail!("mpv 在 IPC 就绪前退出，状态: {status}");
        }
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(Box::new(stream)),
            Err(_) => sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("无法连接 mpv Unix socket {}", path.display())
}

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
    fn maximizes_by_default_and_allows_an_explicit_override() {
        let args = window_args(&[OsString::from("--window-maximized=no")]);

        assert_eq!(args[0], "--window-maximized=yes");
        assert_eq!(args[1], "--window-maximized=no");
        assert!(!window_maximized(&args));
        assert!(window_maximized(&[]));
    }

    #[tokio::test]
    async fn sets_media_and_window_titles_through_ipc() {
        let (mut writer, mut reader) = duplex(1024);
        set_window_state(&mut writer, "剧集名称 S01E02", true)
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
            json!(["set_property", "window-maximized", true])
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
