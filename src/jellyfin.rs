use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, RwLock, mpsc},
    time::{Instant, interval_at, sleep},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::client::{Request as WebSocketRequest, Response as WebSocketResponse},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    protocol::{
        AuthenticationResult, ItemDto, MediaAttachment, MediaStream, PlaybackInfoResponse,
        PlaybackReport, PlaybackStopReport, RemoteCommand, SocketAction, UserDto,
        parse_socket_message,
    },
};

const CLIENT_NAME: &str = "Jellyfin MPV Cast";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_FONT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SUBTITLE_BYTES: u64 = 8 * 1024 * 1024;

pub struct JellyfinClient {
    config: Config,
    auth: RwLock<AuthState>,
    reauthenticate: Mutex<()>,
}

struct AuthState {
    access_token: String,
    http: Client,
    generation: u64,
}

struct FontDirectoryGuard {
    path: Option<PathBuf>,
}

impl FontDirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("字体目录尚未移交")
    }

    fn persist(mut self) -> PathBuf {
        self.path.take().expect("字体目录尚未移交")
    }
}

impl Drop for FontDirectoryGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl JellyfinClient {
    pub async fn connect(config: Config) -> Result<(Self, UserDto)> {
        let result = authenticate_by_name(&config).await?;
        let http = authenticated_http_client(&config, &result.access_token)?;
        let user = result.user;
        let client = Self {
            config,
            auth: RwLock::new(AuthState {
                access_token: result.access_token,
                http,
                generation: 0,
            }),
            reauthenticate: Mutex::new(()),
        };
        Ok((client, user))
    }

    async fn auth_snapshot(&self) -> (String, Client, u64) {
        let auth = self.auth.read().await;
        (
            auth.access_token.clone(),
            auth.http.clone(),
            auth.generation,
        )
    }

    async fn reauthenticate_if_rejected(&self, rejected_token: &str) -> Result<()> {
        let _guard = self.reauthenticate.lock().await;
        if self.auth.read().await.access_token != rejected_token {
            return Ok(());
        }

        warn!("Access Token 已失效，正在使用用户名和密码重新登录");
        let result = authenticate_by_name(&self.config)
            .await
            .context("Access Token 失效后重新登录失败")?;
        let http = authenticated_http_client(&self.config, &result.access_token)?;
        let mut auth = self.auth.write().await;
        auth.access_token = result.access_token;
        auth.http = http;
        auth.generation = auth.generation.wrapping_add(1);
        info!("Access Token 已更新");
        Ok(())
    }

    async fn get_authenticated(&self, url: Url) -> Result<reqwest::Response> {
        let (token, http, _) = self.auth_snapshot().await;
        let response = http
            .get(url.clone())
            .send()
            .await
            .context("HTTP 请求失败")?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        self.reauthenticate_if_rejected(&token).await?;
        let (_, http, _) = self.auth_snapshot().await;
        http.get(url)
            .send()
            .await
            .context("重新登录后重试 HTTP 请求失败")
    }

    async fn post_authenticated<T: Serialize + ?Sized>(
        &self,
        url: Url,
        body: &T,
    ) -> Result<reqwest::Response> {
        let (token, http, _) = self.auth_snapshot().await;
        let response = http
            .post(url.clone())
            .json(body)
            .send()
            .await
            .context("HTTP 请求失败")?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        self.reauthenticate_if_rejected(&token).await?;
        let (_, http, _) = self.auth_snapshot().await;
        http.post(url)
            .json(body)
            .send()
            .await
            .context("重新登录后重试 HTTP 请求失败")
    }

    pub async fn register_capabilities(&self) -> Result<()> {
        let body = json!({
            "PlayableMediaTypes": ["Video", "Audio"],
            "SupportedCommands": [
                "SetVolume", "Mute", "Unmute", "ToggleMute", "SetSubtitleStreamIndex"
            ],
            "SupportsMediaControl": true,
            "SupportsPersistentIdentifier": true
        });
        self.post_json(self.endpoint(&["Sessions", "Capabilities", "Full"])?, &body)
            .await
            .context("注册 Jellyfin 播放设备失败")
    }

    pub async fn item(&self, user_id: &str, item_id: &str) -> Result<ItemDto> {
        self.get_json(self.endpoint(&["Users", user_id, "Items", item_id])?)
            .await
            .with_context(|| format!("获取媒体项目 {item_id} 失败"))
    }

    pub async fn playback_info(
        &self,
        user_id: &str,
        item_id: &str,
        start_ticks: i64,
        media_source_id: Option<&str>,
        audio_index: Option<i32>,
        subtitle_index: Option<i32>,
    ) -> Result<PlaybackInfoResponse> {
        let mut url = self.endpoint(&["Items", item_id, "PlaybackInfo"])?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("userId", user_id);
            query.append_pair("startTimeTicks", &start_ticks.to_string());
            query.append_pair("autoOpenLiveStream", "true");
            query.append_pair("enableDirectPlay", "true");
            query.append_pair("enableDirectStream", "true");
            query.append_pair("enableTranscoding", "true");
            if let Some(id) = media_source_id {
                query.append_pair("mediaSourceId", id);
            }
            if let Some(index) = audio_index {
                query.append_pair("audioStreamIndex", &index.to_string());
            }
            if let Some(index) = subtitle_index {
                query.append_pair("subtitleStreamIndex", &index.to_string());
            }
        }

        let response = self
            .post_authenticated(
                url,
                &playback_profile(
                    user_id,
                    start_ticks,
                    media_source_id,
                    audio_index,
                    subtitle_index,
                ),
            )
            .await
            .context("请求播放信息失败")?;
        let response = ensure_success(response).await?;
        let info: PlaybackInfoResponse = response
            .json()
            .await
            .context("解析 Jellyfin 播放信息失败")?;
        if let Some(error_code) = &info.error_code {
            bail!("Jellyfin 无法提供兼容的媒体流: {error_code}");
        }
        Ok(info)
    }

    pub fn stream_url(
        &self,
        item: &ItemDto,
        source_id: &str,
        play_session_id: &str,
        transcoding_url: Option<&str>,
    ) -> Result<Url> {
        let mut url = if let Some(path) = transcoding_url {
            self.config
                .server
                .join(path.trim_start_matches('/'))
                .context("Jellyfin 返回了非法转码地址")?
        } else {
            let kind = if item.media_type.as_deref() == Some("Audio") {
                "Audio"
            } else {
                "Videos"
            };
            let mut direct = self.endpoint(&[kind, &item.id, "stream"])?;
            direct
                .query_pairs_mut()
                .append_pair("static", "true")
                .append_pair("mediaSourceId", source_id)
                .append_pair("deviceId", self.config.device_id())
                .append_pair("playSessionId", play_session_id);
            direct
        };

        // 某些 Jellyfin 版本会把 api_key 放进 TranscodingUrl。mpv 通过 IPC
        // 收到 X-Emby-Token，因此可安全移除 URL 中的 token。
        strip_api_key(&mut url);
        Ok(url)
    }

    pub fn subtitle_url(
        &self,
        item_id: &str,
        source_id: &str,
        stream: &MediaStream,
    ) -> Result<Option<Url>> {
        if stream.stream_type != "Subtitle" {
            return Ok(None);
        }
        let externally_delivered =
            stream.delivery_method.as_deref() == Some("External") || stream.is_text_subtitle_stream;
        if !externally_delivered {
            return Ok(None);
        }

        let mut url = if let Some(path) = stream.delivery_url.as_deref() {
            self.config
                .server
                .join(path.trim_start_matches('/'))
                .context("Jellyfin 返回了非法字幕地址")?
        } else if stream.is_text_subtitle_stream {
            let format = match stream.codec.as_deref() {
                Some("ass") => "ass",
                Some("ssa") => "ssa",
                Some("webvtt" | "vtt") => "vtt",
                _ => "srt",
            };
            let index = stream.index.to_string();
            let filename = format!("Stream.{format}");
            self.endpoint(&["Videos", item_id, source_id, "Subtitles", &index, &filename])?
        } else {
            return Ok(None);
        };
        strip_api_key(&mut url);
        Ok(Some(url))
    }

    pub async fn download_fonts(
        &self,
        item_id: &str,
        source_id: &str,
        attachments: &[MediaAttachment],
        subtitle_url: &Url,
    ) -> Option<PathBuf> {
        let required_fonts = match self.required_ass_fonts(subtitle_url).await {
            Ok(fonts) => fonts,
            Err(error) => {
                warn!(%error, "无法分析 ASS/SSA 字幕所需字体");
                return None;
            }
        };
        if required_fonts.is_empty() {
            debug!("ASS/SSA 字幕没有声明字体，跳过字体下载");
            return None;
        }
        let mut required_font_names = required_fonts.iter().cloned().collect::<Vec<_>>();
        required_font_names.sort_unstable();
        info!(fonts = ?required_font_names, "已解析 ASS/SSA 字幕所需字体");
        let fonts: Vec<(&MediaAttachment, &str)> = attachments
            .iter()
            .filter(|attachment| {
                attachment
                    .file_name
                    .as_deref()
                    .is_some_and(|name| font_file_is_required(name, &required_fonts))
            })
            .filter_map(|attachment| font_extension(attachment).map(|ext| (attachment, ext)))
            .collect();
        if fonts.is_empty() {
            debug!("媒体中没有与字幕声明匹配的字体附件");
            return None;
        }

        let directory_path =
            std::env::temp_dir().join(format!("jellyfin-mpv-fonts-{}", Uuid::new_v4()));
        if let Err(error) = tokio::fs::create_dir(&directory_path).await {
            warn!(%error, path = %directory_path.display(), "无法创建字幕字体临时目录");
            return None;
        }
        let directory = FontDirectoryGuard::new(directory_path);

        let mut total_bytes = 0_u64;
        let mut downloaded = 0_usize;
        for (attachment, extension) in fonts {
            match self
                .download_font(
                    item_id,
                    source_id,
                    attachment,
                    extension,
                    directory.path(),
                    &mut total_bytes,
                )
                .await
            {
                Ok(path) => {
                    downloaded += 1;
                    debug!(path = %path.display(), "已下载 Jellyfin 字体附件");
                }
                Err(error) => {
                    warn!(index = attachment.index, %error, "下载 Jellyfin 字体附件失败");
                }
            }
        }

        if downloaded == 0 {
            return None;
        }
        info!(
            downloaded,
            bytes = total_bytes,
            required = required_fonts.len(),
            "Jellyfin 所需字幕字体已就绪"
        );
        Some(directory.persist())
    }

    async fn required_ass_fonts(&self, subtitle_url: &Url) -> Result<HashSet<String>> {
        let response = ensure_success(self.get_authenticated(subtitle_url.clone()).await?).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SUBTITLE_BYTES)
        {
            bail!("字幕文件超过 8 MiB 限制");
        }
        let bytes = response.bytes().await.context("读取 ASS/SSA 字幕失败")?;
        if bytes.len() as u64 > MAX_SUBTITLE_BYTES {
            bail!("字幕文件超过 8 MiB 限制");
        }
        Ok(parse_ass_font_names(&String::from_utf8_lossy(&bytes)))
    }

    async fn download_font(
        &self,
        item_id: &str,
        source_id: &str,
        attachment: &MediaAttachment,
        extension: &str,
        directory: &Path,
        total_bytes: &mut u64,
    ) -> Result<PathBuf> {
        let mut url = if let Some(path) = attachment.delivery_url.as_deref() {
            self.config
                .server
                .join(path.trim_start_matches('/'))
                .context("Jellyfin 返回了非法字体附件地址")?
        } else {
            let index = attachment.index.to_string();
            self.endpoint(&["Videos", item_id, source_id, "Attachments", &index])?
        };
        strip_api_key(&mut url);
        let response = ensure_success(self.get_authenticated(url).await?).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_FONT_BYTES)
        {
            bail!("字体附件超过 32 MiB 限制");
        }
        let bytes = response.bytes().await.context("读取字体附件失败")?;
        let length = bytes.len() as u64;
        if length > MAX_FONT_BYTES {
            bail!("字体附件超过 32 MiB 限制");
        }
        if total_bytes.saturating_add(length) > MAX_TOTAL_FONT_BYTES {
            bail!("本次播放的字体附件总量超过 128 MiB 限制");
        }

        let file_name = safe_font_file_name(attachment, extension);
        let path = directory.join(file_name);
        tokio::fs::write(&path, bytes)
            .await
            .context("写入字体附件失败")?;
        *total_bytes += length;
        Ok(path)
    }

    pub async fn mpv_http_headers(
        &self,
        required: &std::collections::HashMap<String, Option<String>>,
    ) -> (u64, Vec<String>) {
        let (token, _, generation) = self.auth_snapshot().await;
        let mut headers = vec![format!("X-Emby-Token: {token}")];
        headers.extend(
            required
                .iter()
                .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}: {value}"))),
        );
        (generation, headers)
    }

    pub async fn report_start(&self, report: &PlaybackReport<'_>) -> Result<()> {
        self.post_json(self.endpoint(&["Sessions", "Playing"])?, report)
            .await
    }

    pub async fn report_progress(&self, report: &PlaybackReport<'_>) -> Result<()> {
        self.post_json(self.endpoint(&["Sessions", "Playing", "Progress"])?, report)
            .await
    }

    pub async fn report_stop(&self, report: &PlaybackStopReport<'_>) -> Result<()> {
        self.post_json(self.endpoint(&["Sessions", "Playing", "Stopped"])?, report)
            .await
    }

    pub async fn websocket_supervisor(
        self: Arc<Self>,
        commands: mpsc::Sender<RemoteCommand>,
        shutdown: CancellationToken,
    ) {
        let mut delay = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            match self.websocket_once(&commands, &shutdown).await {
                Ok(()) if shutdown.is_cancelled() || commands.is_closed() => break,
                Ok(()) => warn!("Jellyfin WebSocket 已断开，准备重连"),
                Err(error) => warn!(%error, "Jellyfin WebSocket 连接失败，准备重连"),
            }

            tokio::select! {
                () = shutdown.cancelled() => break,
                () = sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    }

    async fn websocket_once(
        &self,
        commands: &mpsc::Sender<RemoteCommand>,
        shutdown: &CancellationToken,
    ) -> Result<()> {
        let mut retried_authentication = false;
        let (mut socket, _) = loop {
            let (token, _, _) = self.auth_snapshot().await;
            let request = websocket_request(&self.config, &token)?;
            match connect_async(request).await {
                Ok(connection) => break connection,
                Err(tokio_tungstenite::tungstenite::Error::Http(response))
                    if !retried_authentication
                        && matches!(
                            response.status(),
                            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                        ) =>
                {
                    warn!(status = %response.status(), "WebSocket 鉴权被拒绝，正在重新登录");
                    self.reauthenticate_if_rejected(&token).await?;
                    retried_authentication = true;
                }
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    bail!(websocket_http_error(&response));
                }
                Err(error) => return Err(error).context("WebSocket 握手失败"),
            }
        };
        info!("Jellyfin WebSocket 已连接");

        // WebSocket 建立后再更新一次，确保当前 session 可被远程控制。
        if let Err(error) = self.register_capabilities().await {
            warn!(%error, "WebSocket 连接后更新设备能力失败");
        }

        let mut keepalive = interval_at(
            Instant::now() + Duration::from_hours(1),
            Duration::from_hours(1),
        );
        let mut keepalive_enabled = false;

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
                message = socket.next() => {
                    let Some(message) = message else { return Ok(()); };
                    match message.context("读取 WebSocket 消息失败")? {
                        Message::Text(text) => match parse_socket_message(&text) {
                            Ok(SocketAction::Command(command)) => {
                                info!(command = command.name(), "收到 Jellyfin 远程命令");
                                if commands.send(command).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(SocketAction::ForceKeepAlive(timeout_seconds)) => {
                                send_keepalive(&mut socket).await?;
                                let period = Duration::from_secs((timeout_seconds / 2).max(1));
                                keepalive = interval_at(Instant::now() + period, period);
                                keepalive_enabled = true;
                                info!(period_seconds = period.as_secs(), "Jellyfin WebSocket KeepAlive 已启用");
                            }
                            Ok(SocketAction::Ignore(message_type)) => {
                                debug!(%message_type, "忽略未支持的 Jellyfin WebSocket 消息");
                            }
                            Err(error) => error!(%error, "无法解析 Jellyfin WebSocket 消息"),
                        },
                        Message::Ping(data) => socket.send(Message::Pong(data)).await?,
                        Message::Close(frame) => {
                            warn!(?frame, "Jellyfin WebSocket 收到关闭帧");
                            return Ok(());
                        }
                        Message::Binary(data) => {
                            warn!(bytes = data.len(), "收到未支持的 Jellyfin WebSocket 二进制消息");
                        }
                        _ => {}
                    }
                }
                _ = keepalive.tick(), if keepalive_enabled => {
                    send_keepalive(&mut socket).await?;
                }
            }
        }
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.config.server.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|()| anyhow!("Jellyfin 根地址不能作为 API 地址"))?;
            path.pop_if_empty();
            path.extend(segments);
        }
        Ok(url)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        let response = self.get_authenticated(url).await?;
        ensure_success(response)
            .await?
            .json()
            .await
            .context("解析 Jellyfin JSON 响应失败")
    }

    async fn post_json<T: Serialize + ?Sized>(&self, url: Url, body: &T) -> Result<()> {
        let response = self.post_authenticated(url, body).await?;
        ensure_success(response).await?;
        Ok(())
    }
}

async fn send_keepalive<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let keepalive = json!({
        "MessageType": "KeepAlive",
        "MessageId": Uuid::new_v4().to_string()
    });
    socket
        .send(Message::Text(keepalive.to_string().into()))
        .await
        .context("发送 WebSocket KeepAlive 失败")
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let summary = body.chars().take(300).collect::<String>();
    if status == StatusCode::UNAUTHORIZED {
        bail!("Jellyfin 拒绝 Access Token (401): {summary}");
    }
    bail!("Jellyfin HTTP {status}: {summary}")
}

fn escape_auth(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn strip_api_key(url: &mut Url) {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("api_key"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
}

fn font_extension(attachment: &MediaAttachment) -> Option<&'static str> {
    let extension = attachment
        .file_name
        .as_deref()
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str());
    for candidate in [extension, attachment.codec.as_deref()] {
        if let Some(extension) = candidate.and_then(supported_font_extension) {
            return Some(extension);
        }
    }

    match attachment
        .mime_type
        .as_deref()?
        .to_ascii_lowercase()
        .as_str()
    {
        "application/x-truetype-font" | "application/font-sfnt" | "font/ttf" => Some("ttf"),
        "application/vnd.ms-opentype" | "font/otf" => Some("otf"),
        "font/collection" => Some("ttc"),
        "font/woff" | "application/font-woff" => Some("woff"),
        "font/woff2" => Some("woff2"),
        _ => None,
    }
}

fn supported_font_extension(extension: &str) -> Option<&'static str> {
    ["ttf", "otf", "ttc", "otc", "woff", "woff2"]
        .into_iter()
        .find(|supported| extension.eq_ignore_ascii_case(supported))
}

fn parse_ass_font_names(text: &str) -> HashSet<String> {
    let mut fonts = HashSet::new();
    let mut in_style_section = false;
    let mut font_name_index = 1_usize;

    for raw_line in text.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}');
        if line.starts_with('[') && line.ends_with(']') {
            in_style_section = line.eq_ignore_ascii_case("[V4 Styles]")
                || line.eq_ignore_ascii_case("[V4+ Styles]");
            continue;
        }

        if in_style_section {
            if let Some(format) = strip_ass_prefix(line, "Format:") {
                if let Some(index) = format
                    .split(',')
                    .position(|field| field.trim().eq_ignore_ascii_case("Fontname"))
                {
                    font_name_index = index;
                }
            } else if let Some(style) = strip_ass_prefix(line, "Style:")
                && let Some(font_name) = style.split(',').nth(font_name_index)
            {
                insert_ass_font_name(&mut fonts, font_name);
            }
        }

        let mut remaining = line;
        while let Some(position) = remaining.find("\\fn") {
            let value = &remaining[position + 3..];
            let end = value.find(['\\', '}']).unwrap_or(value.len());
            insert_ass_font_name(&mut fonts, &value[..end]);
            remaining = value.get(end.saturating_add(1)..).unwrap_or_default();
        }
    }

    fonts
}

fn strip_ass_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = line.get(..prefix.len())?;
    candidate
        .eq_ignore_ascii_case(prefix)
        .then_some(&line[prefix.len()..])
}

fn insert_ass_font_name(fonts: &mut HashSet<String>, value: &str) {
    let name = value.trim().trim_start_matches('@').trim();
    if !name.is_empty() {
        fonts.insert(name.to_owned());
    }
}

fn font_file_is_required(file_name: &str, required_fonts: &HashSet<String>) -> bool {
    required_fonts
        .iter()
        .any(|font_name| font_file_matches_required(file_name, font_name))
}

fn font_file_matches_required(file_name: &str, required_font: &str) -> bool {
    let candidate = normalize_font_name(&safe_file_stem(file_name));
    let required = normalize_font_name(required_font);
    !candidate.is_empty()
        && !required.is_empty()
        && (candidate == required
            || candidate
                .strip_prefix(&required)
                .is_some_and(is_font_style_suffix))
}

fn normalize_font_name(name: &str) -> String {
    name.trim_start_matches('@')
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_font_style_suffix(suffix: &str) -> bool {
    matches!(
        suffix,
        "regular"
            | "normal"
            | "roman"
            | "book"
            | "medium"
            | "light"
            | "thin"
            | "bold"
            | "black"
            | "heavy"
            | "semibold"
            | "demibold"
            | "italic"
            | "oblique"
            | "bolditalic"
            | "boldoblique"
            | "condensed"
            | "condensedregular"
            | "condensedbold"
            | "condenseditalic"
    )
}

fn safe_font_file_name(attachment: &MediaAttachment, extension: &str) -> String {
    let name = attachment.file_name.as_deref().unwrap_or("font");
    format!(
        "{}-{}.{}",
        attachment.index,
        safe_file_stem(name),
        extension
    )
}

fn safe_file_stem(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("font");
    stem.chars()
        .take(100)
        .map(|character| {
            if character.is_control() || r#"<>:"/\|?*"#.contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect()
}

async fn authenticate_by_name(config: &Config) -> Result<AuthenticationResult> {
    let http = Client::builder()
        .default_headers(auth_headers(config, None)?)
        .timeout(Duration::from_secs(30))
        .build()
        .context("创建 Jellyfin 登录客户端失败")?;
    let url = config
        .server
        .join("Users/AuthenticateByName")
        .context("无法构造 Jellyfin 登录地址")?;
    let response = http
        .post(url)
        .json(&json!({
            "Username": config.username.as_deref().expect("已验证用户名"),
            "Pw": config.password.as_deref().expect("已验证密码")
        }))
        .send()
        .await
        .context("连接 Jellyfin 登录接口失败")?;
    if response.status() == StatusCode::UNAUTHORIZED {
        let detail = response
            .text()
            .await
            .unwrap_or_default()
            .trim()
            .chars()
            .take(500)
            .collect::<String>();
        if detail.is_empty() {
            bail!(
                "Jellyfin 拒绝登录（401 Unauthorized）；密码已成功从 Base64 解码为 UTF-8，请检查用户名、账户状态和 Jellyfin 服务端日志"
            );
        }
        bail!(
            "Jellyfin 拒绝登录（401 Unauthorized）；密码已成功从 Base64 解码为 UTF-8。服务器响应：{detail}"
        );
    }
    ensure_success(response)
        .await?
        .json()
        .await
        .context("解析 Jellyfin 登录响应失败")
}

fn authenticated_http_client(config: &Config, access_token: &str) -> Result<Client> {
    Client::builder()
        .default_headers(auth_headers(config, Some(access_token))?)
        .timeout(Duration::from_secs(30))
        .build()
        .context("创建 Jellyfin HTTP 客户端失败")
}

fn auth_headers(config: &Config, token: Option<&str>) -> Result<HeaderMap> {
    let token_part = token
        .map(|token| format!(", Token=\"{}\"", escape_auth(token)))
        .unwrap_or_default();
    let auth = format!(
        "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"{}",
        CLIENT_NAME,
        escape_auth(config.device_name()),
        escape_auth(config.device_id()),
        CLIENT_VERSION,
        token_part,
    );
    let mut headers = HeaderMap::new();
    let auth =
        HeaderValue::from_str(&auth).context("用户名、Token 或设备名称含有非法 HTTP 字符")?;
    headers.insert(AUTHORIZATION, auth.clone());
    headers.insert("X-Emby-Authorization", auth);
    if let Some(token) = token {
        headers.insert(
            "X-Emby-Token",
            HeaderValue::from_str(token).context("Access Token 含有非法 HTTP 字符")?,
        );
    }
    Ok(headers)
}

fn websocket_request(config: &Config, token: &str) -> Result<WebSocketRequest> {
    let mut url = config.server.clone();
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|()| anyhow!("无法构造 WebSocket 地址"))?;
    url = url.join("socket").context("无法构造 WebSocket 地址")?;
    url.query_pairs_mut()
        .append_pair("api_key", token)
        .append_pair("deviceId", config.device_id());

    let mut request = url
        .as_str()
        .into_client_request()
        .context("无法构造 WebSocket 握手请求")?;
    request
        .headers_mut()
        .extend(auth_headers(config, Some(token))?);
    Ok(request)
}

fn websocket_http_error(response: &WebSocketResponse) -> String {
    let body = response
        .body()
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let summary = body.chars().take(300).collect::<String>();
    if summary.is_empty() {
        format!("WebSocket 握手被服务器拒绝: HTTP {}", response.status())
    } else {
        format!(
            "WebSocket 握手被服务器拒绝: HTTP {}: {summary}",
            response.status()
        )
    }
}

fn playback_profile(
    user_id: &str,
    start_ticks: i64,
    media_source_id: Option<&str>,
    audio_index: Option<i32>,
    subtitle_index: Option<i32>,
) -> Value {
    json!({
        "UserId": user_id,
        "StartTimeTicks": start_ticks,
        "MediaSourceId": media_source_id,
        "AudioStreamIndex": audio_index,
        "SubtitleStreamIndex": subtitle_index,
        "EnableDirectPlay": true,
        "EnableDirectStream": true,
        "EnableTranscoding": true,
        "AllowVideoStreamCopy": true,
        "AllowAudioStreamCopy": true,
        "AutoOpenLiveStream": true,
        "DeviceProfile": {
            "Name": "mpv",
            "MaxStreamingBitrate": 2_147_483_647_i64,
            "MaxStaticBitrate": 2_147_483_647_i64,
            "DirectPlayProfiles": [
                {
                    "Type": "Video",
                    "Container": "avi,mkv,mov,mp4,m4v,webm,ts,mpegts,m2ts,mpg,mpeg,wmv,asf,flv,ogv,3gp",
                    "VideoCodec": "h264,hevc,mpeg4,mpeg2video,vp8,vp9,av1,vc1,theora",
                    "AudioCodec": "aac,ac3,eac3,dts,truehd,mp3,flac,opus,vorbis,pcm_s16le,pcm_s24le"
                },
                {
                    "Type": "Audio",
                    "Container": "mp3,aac,m4a,flac,ogg,oga,opus,wav,wma,ape,alac,mka",
                    "AudioCodec": "aac,mp3,flac,opus,vorbis,wmav2,ape,alac,pcm_s16le,pcm_s24le"
                }
            ],
            "TranscodingProfiles": [
                {
                    "Type": "Video", "Container": "ts", "Protocol": "hls",
                    "VideoCodec": "h264", "AudioCodec": "aac", "Context": "Streaming"
                },
                {
                    "Type": "Audio", "Container": "mp3", "Protocol": "http",
                    "AudioCodec": "mp3", "Context": "Streaming"
                }
            ],
            "SubtitleProfiles": [
                { "Format": "srt", "Method": "External" },
                { "Format": "ass", "Method": "External" },
                { "Format": "ssa", "Method": "External" },
                { "Format": "vtt", "Method": "External" },
                { "Format": "sub", "Method": "Embed" },
                { "Format": "pgs", "Method": "Embed" },
                { "Format": "dvdsub", "Method": "Embed" }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use clap::Parser;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    use super::*;
    use crate::config::ConfigArgs;

    #[test]
    fn websocket_handshake_uses_query_and_standard_authentication() {
        let encoded = STANDARD.encode("password");
        let config = ConfigArgs::try_parse_from([
            "jellyfin-mpv",
            "--server",
            "http://localhost:8096",
            "--username",
            "Alice",
            "--password",
            &encoded,
            "--device-id",
            "test-device",
        ])
        .unwrap()
        .resolve()
        .unwrap()
        .unwrap();

        let request = websocket_request(&config, "fresh-token").unwrap();
        let uri = request.uri().to_string();
        assert!(uri.contains("api_key=fresh-token"));
        assert!(uri.contains("deviceId=test-device"));
        let authorization = request.headers()[AUTHORIZATION].to_str().unwrap();
        assert!(authorization.starts_with("MediaBrowser "));
        assert!(authorization.contains("Token=\"fresh-token\""));
        assert_eq!(request.headers()["X-Emby-Token"], "fresh-token");
    }

    #[test]
    fn recognizes_only_supported_font_attachments() {
        let font = MediaAttachment {
            index: 4,
            codec: Some("ttf".to_owned()),
            file_name: Some("../unsafe:font.TTF".to_owned()),
            mime_type: None,
            delivery_url: None,
        };
        let cover = MediaAttachment {
            index: 5,
            codec: Some("mjpeg".to_owned()),
            file_name: Some("cover.jpg".to_owned()),
            mime_type: Some("image/jpeg".to_owned()),
            delivery_url: None,
        };
        assert_eq!(font_extension(&font), Some("ttf"));
        assert_eq!(safe_font_file_name(&font, "ttf"), "4-unsafe_font.ttf");
        assert_eq!(font_extension(&cover), None);
    }

    #[test]
    fn parses_ass_style_and_inline_font_names() {
        let subtitle = r#"
[V4+ Styles]
Format: Name, Fontname, Fontsize, Bold
Style: Default, Noto Sans CJK SC, 20, 0
[Events]
Format: Layer, Start, End, Style, Text
Dialogue: 0,0:00:00.00,0:00:01.00,Default,{\fnSource Han Sans CN}正文
Dialogue: 0,0:00:01.00,0:00:02.00,Default,{\fn@方正黑体\b1}竖排
"#;

        let fonts = parse_ass_font_names(subtitle);

        assert!(fonts.contains("Noto Sans CJK SC"));
        assert!(fonts.contains("Source Han Sans CN"));
        assert!(fonts.contains("方正黑体"));
        assert_eq!(fonts.len(), 3);
    }

    #[test]
    fn matches_only_required_font_files_and_style_variants() {
        let required = HashSet::from(["Noto Sans CJK SC".to_owned(), "方正黑体".to_owned()]);

        assert!(font_file_is_required(
            "NotoSansCJKSC-Regular.otf",
            &required
        ));
        assert!(font_file_is_required("方正黑体.ttf", &required));
        assert!(!font_file_is_required("NotoSerifCJKSC.otf", &required));
        assert!(!font_file_is_required("Unneeded.ttf", &required));
    }

    #[tokio::test]
    async fn downloads_font_attachment_from_jellyfin() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let responses = [
                (
                    200,
                    r#"{"AccessToken":"token","User":{"Id":"user-1","Name":"Alice"}}"#,
                ),
                (
                    200,
                    "[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default, Example, 20",
                ),
                (200, "fake-font-data"),
            ];
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                write_response(&mut stream, status, body).await;
            }
            requests
        });

        let encoded = STANDARD.encode("password");
        let config = ConfigArgs::try_parse_from([
            "jellyfin-mpv",
            "--server",
            &format!("http://{address}"),
            "--username",
            "Alice",
            "--password",
            &encoded,
            "--device-id",
            "test-device",
        ])
        .unwrap()
        .resolve()
        .unwrap()
        .unwrap();
        let (client, _) = JellyfinClient::connect(config).await.unwrap();
        let attachment = MediaAttachment {
            index: 4,
            codec: Some("ttf".to_owned()),
            file_name: Some("Example.ttf".to_owned()),
            mime_type: Some("font/ttf".to_owned()),
            delivery_url: None,
        };
        let subtitle_url = client.endpoint(&["subtitle.ass"]).unwrap();
        let directory = client
            .download_fonts("item-1", "source-1", &[attachment], &subtitle_url)
            .await
            .unwrap();
        let bytes = tokio::fs::read(directory.join("4-Example.ttf"))
            .await
            .unwrap();
        assert_eq!(bytes, b"fake-font-data");
        tokio::fs::remove_dir_all(directory).await.unwrap();

        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /subtitle.ass "));
        assert!(requests[2].starts_with("GET /Videos/item-1/source-1/Attachments/4 "));
    }

    #[tokio::test]
    async fn relogs_in_and_retries_after_unauthorized() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let responses = [
                (
                    200,
                    r#"{"AccessToken":"old-token","User":{"Id":"user-1","Name":"Alice"}}"#,
                ),
                (401, r#"{"error":"invalid token"}"#),
                (
                    200,
                    r#"{"AccessToken":"new-token","User":{"Id":"user-1","Name":"Alice"}}"#,
                ),
                (200, r#"{"Id":"item-1","Name":"Movie","MediaType":"Video"}"#),
            ];
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                write_response(&mut stream, status, body).await;
            }
            requests
        });

        let encoded = STANDARD.encode("password");
        let config = ConfigArgs::try_parse_from([
            "jellyfin-mpv",
            "--server",
            &format!("http://{address}"),
            "--username",
            "Alice",
            "--password",
            &encoded,
            "--device-id",
            "test-device",
        ])
        .unwrap()
        .resolve()
        .unwrap()
        .unwrap();
        let (client, _) = JellyfinClient::connect(config).await.unwrap();
        let item = client.item("user-1", "item-1").await.unwrap();
        assert_eq!(item.name, "Movie");

        let requests = server.await.unwrap();
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-emby-token: old-token")
        );
        assert!(
            requests[3]
                .to_ascii_lowercase()
                .contains("x-emby-token: new-token")
        );
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if expected_len.is_none()
                && let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|length| request.len() >= length) {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    async fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
        let reason = if status == 200 { "OK" } else { "Unauthorized" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}
