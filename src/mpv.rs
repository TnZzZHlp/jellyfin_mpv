use std::{
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
    FileLoaded { id: Uuid },
    Position { id: Uuid, seconds: f64 },
    Paused { id: Uuid, paused: bool },
    Muted { id: Uuid, muted: bool },
    Volume { id: Uuid, volume: f64 },
    Ended { id: Uuid, failed: bool },
    Terminated { id: Uuid, failed: bool },
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
    Quit,
}

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
        events,
    } = launch;
    let id = Uuid::new_v4();
    let ipc_path = ipc_path(id);
    let window_maximized = window_maximized(&extra_args);
    let mut command = Command::new(&executable);
    command.args(window_args(&extra_args));
    if let Some(directory) = font_directory {
        command.arg(format!("--sub-fonts-dir={}", directory.to_string_lossy()));
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

    let child = command
        .spawn()
        .with_context(|| format!("无法启动 mpv: {}", executable.display()))?;
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
    mut commands: mpsc::Receiver<MpvCommand>,
    events: mpsc::Sender<MpvEvent>,
    startup: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    let stream = connect_ipc(&ipc_path, &mut child).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

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
                let should_quit = matches!(command, MpvCommand::Quit);
                send_mpv_command(&mut writer, command).await?;
                if should_quit {
                    break;
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
                            let _ = events.send(MpvEvent::FileLoaded { id }).await;
                        } else if value.get("event").and_then(Value::as_str) == Some("property-change") {
                            emit_property(id, &value, &events).await;
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
}
