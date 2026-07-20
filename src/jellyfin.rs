use std::{
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, OnceCell, RwLock, mpsc},
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

pub struct JellyfinClient {
    config: Config,
    auth: RwLock<AuthState>,
    reauthenticate: Mutex<()>,
    fallback_fonts: OnceCell<Vec<CachedFont>>,
}

struct AuthState {
    access_token: String,
    http: Client,
    generation: u64,
}

struct CachedFont {
    file_name: String,
    bytes: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FallbackFontFile {
    name: String,
    size: u64,
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
            fallback_fonts: OnceCell::new(),
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
    ) -> Option<PathBuf> {
        let fonts: Vec<(&MediaAttachment, &str)> = attachments
            .iter()
            .filter_map(|attachment| font_extension(attachment).map(|ext| (attachment, ext)))
            .collect();
        let fallback_fonts = match self
            .fallback_fonts
            .get_or_try_init(|| self.fetch_fallback_fonts())
            .await
        {
            Ok(fonts) => fonts.as_slice(),
            Err(error) => {
                warn!(%error, "获取 Jellyfin 服务器备用字体失败");
                &[]
            }
        };
        if fonts.is_empty() && fallback_fonts.is_empty() {
            return None;
        }

        let directory = std::env::temp_dir().join(format!("jellyfin-mpv-fonts-{}", Uuid::new_v4()));
        if let Err(error) = tokio::fs::create_dir(&directory).await {
            warn!(%error, path = %directory.display(), "无法创建字幕字体临时目录");
            return None;
        }

        let mut total_bytes = 0_u64;
        let mut downloaded = 0_usize;
        for font in fallback_fonts {
            let length = font.bytes.len() as u64;
            if total_bytes.saturating_add(length) > MAX_TOTAL_FONT_BYTES {
                warn!("服务器备用字体总量超过 128 MiB，忽略剩余字体");
                break;
            }
            let path = directory.join(&font.file_name);
            match tokio::fs::write(&path, &font.bytes).await {
                Ok(()) => {
                    downloaded += 1;
                    total_bytes += length;
                }
                Err(error) => warn!(%error, path = %path.display(), "写入服务器备用字体失败"),
            }
        }
        for (attachment, extension) in fonts {
            match self
                .download_font(
                    item_id,
                    source_id,
                    attachment,
                    extension,
                    &directory,
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
            let _ = tokio::fs::remove_dir_all(&directory).await;
            return None;
        }
        info!(
            downloaded,
            bytes = total_bytes,
            "Jellyfin 备用字幕字体已就绪"
        );
        Some(directory)
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

    async fn fetch_fallback_fonts(&self) -> Result<Vec<CachedFont>> {
        let files: Vec<FallbackFontFile> = self
            .get_json(self.endpoint(&["FallbackFont", "Fonts"])?)
            .await?;
        let mut fonts = Vec::new();
        let mut total_bytes = 0_u64;
        for file in files {
            let Some(extension) = Path::new(&file.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(supported_font_extension)
            else {
                continue;
            };
            if file.size == 0 || file.size > MAX_FONT_BYTES {
                warn!(name = %file.name, size = file.size, "忽略大小异常的服务器备用字体");
                continue;
            }
            if total_bytes.saturating_add(file.size) > MAX_TOTAL_FONT_BYTES {
                warn!("服务器备用字体总量超过 128 MiB，忽略剩余字体");
                break;
            }

            let response = match self
                .get_authenticated(self.endpoint(&["FallbackFont", "Fonts", &file.name])?)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    warn!(name = %file.name, %error, "下载服务器备用字体失败");
                    continue;
                }
            };
            let response = match ensure_success(response).await {
                Ok(response) => response,
                Err(error) => {
                    warn!(name = %file.name, %error, "下载服务器备用字体失败");
                    continue;
                }
            };
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(name = %file.name, %error, "读取服务器备用字体失败");
                    continue;
                }
            };
            let length = bytes.len() as u64;
            if length == 0
                || length > MAX_FONT_BYTES
                || total_bytes.saturating_add(length) > MAX_TOTAL_FONT_BYTES
            {
                warn!(name = %file.name, size = length, "忽略大小异常的服务器备用字体");
                continue;
            }
            fonts.push(CachedFont {
                file_name: format!("fallback-{}.{}", safe_file_stem(&file.name), extension),
                bytes: bytes.to_vec(),
            });
            total_bytes += length;
        }
        info!(
            count = fonts.len(),
            bytes = total_bytes,
            "已缓存 Jellyfin 服务器备用字体"
        );
        Ok(fonts)
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
                                if commands.send(command).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(SocketAction::ForceKeepAlive(timeout_seconds)) => {
                                send_keepalive(&mut socket).await?;
                                let period = Duration::from_secs((timeout_seconds / 2).max(1));
                                keepalive = interval_at(Instant::now() + period, period);
                                keepalive_enabled = true;
                            }
                            Ok(SocketAction::Ignore) => debug!("忽略未支持的 Jellyfin WebSocket 消息"),
                            Err(error) => error!(%error, "无法解析 Jellyfin WebSocket 消息"),
                        },
                        Message::Ping(data) => socket.send(Message::Pong(data)).await?,
                        Message::Close(_) => return Ok(()),
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
                (200, "[]"),
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
        let directory = client
            .download_fonts("item-1", "source-1", &[attachment])
            .await
            .unwrap();
        let bytes = tokio::fs::read(directory.join("4-Example.ttf"))
            .await
            .unwrap();
        assert_eq!(bytes, b"fake-font-data");
        tokio::fs::remove_dir_all(directory).await.unwrap();

        let requests = server.await.unwrap();
        assert!(requests[2].starts_with("GET /Videos/item-1/source-1/Attachments/4 "));
    }

    #[tokio::test]
    async fn downloads_and_caches_server_fallback_fonts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let responses = [
                (
                    200,
                    r#"{"AccessToken":"token","User":{"Id":"user-1","Name":"Alice"}}"#,
                ),
                (200, r#"[{"Name":"Fallback.ttf","Size":14}]"#),
                (200, "fallback-font"),
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
        let directory = client
            .download_fonts("item-1", "source-1", &[])
            .await
            .unwrap();
        let bytes = tokio::fs::read(directory.join("fallback-Fallback.ttf"))
            .await
            .unwrap();
        assert_eq!(bytes, b"fallback-font");
        tokio::fs::remove_dir_all(directory).await.unwrap();

        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /FallbackFont/Fonts "));
        assert!(requests[2].starts_with("GET /FallbackFont/Fonts/Fallback.ttf "));
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
