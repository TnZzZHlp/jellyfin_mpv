use std::{sync::Arc, time::Duration};

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
        AuthenticationResult, ItemDto, MediaSegment, MediaSegmentQueryResult, MediaStream,
        PlaybackInfoResponse, PlaybackReport, PlaybackStopReport, RemoteCommand, SocketAction,
        UserDto, parse_socket_message,
    },
    skip_settings::ALL_SEGMENT_TYPES,
};

const CLIENT_NAME: &str = "Jellyfin MPV Cast";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_TRICKPLAY_TILE_BYTES: u64 = 32 * 1024 * 1024;
// Jellyfin 10.7+ binds these fields as System.Int32.
const MAX_JELLYFIN_STREAMING_BITRATE: i64 = i32::MAX as i64;

pub struct JellyfinClient {
    config: Config,
    auth: RwLock<AuthState>,
    reauthenticate: Mutex<()>,
}

fn session_capabilities() -> Value {
    json!({
        "PlayableMediaTypes": ["Video", "Audio"],
        // "PlaybackRate" 不是 Jellyfin GeneralCommandType，不能在此通告。
        "SupportedCommands": [
            "PlayMediaSource",
            "SetVolume", "VolumeUp", "VolumeDown", "ToggleFullscreen",
            "Mute", "Unmute", "ToggleMute",
            "SetAudioStreamIndex", "SetSubtitleStreamIndex"
        ],
        "SupportsMediaControl": true,
        "SupportsPersistentIdentifier": true
    })
}

struct AuthState {
    access_token: String,
    http: Client,
    generation: u64,
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
        self.post_json(
            self.endpoint(&["Sessions", "Capabilities", "Full"])?,
            &session_capabilities(),
        )
        .await
        .context("注册 Jellyfin 播放设备失败")
    }

    pub async fn item(&self, user_id: &str, item_id: &str) -> Result<ItemDto> {
        let mut url = self.endpoint(&["Users", user_id, "Items", item_id])?;
        url.query_pairs_mut().append_pair("fields", "Trickplay");
        self.get_json(url)
            .await
            .with_context(|| format!("获取媒体项目 {item_id} 失败"))
    }

    pub async fn trickplay_tile(
        &self,
        item_id: &str,
        media_source_id: &str,
        width: u32,
        index: u32,
    ) -> Result<Option<Vec<u8>>> {
        let width = width.to_string();
        let index = format!("{index}.jpg");
        let mut url = self.endpoint(&["Videos", item_id, "Trickplay", &width, &index])?;
        url.query_pairs_mut()
            .append_pair("mediaSourceId", media_source_id);
        let response = self
            .get_authenticated(url)
            .await
            .with_context(|| format!("下载 Trickplay 雪碧图 {index} 失败"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut response = ensure_success(response).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_TRICKPLAY_TILE_BYTES)
        {
            bail!("Trickplay 雪碧图超过 32 MiB 限制");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("读取 Trickplay 雪碧图失败")?
        {
            if bytes.len() as u64 + chunk.len() as u64 > MAX_TRICKPLAY_TILE_BYTES {
                bail!("Trickplay 雪碧图超过 32 MiB 限制");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }

    pub async fn media_segments(&self, item_id: &str) -> Result<Vec<MediaSegment>> {
        let mut url = self.endpoint(&["MediaSegments", item_id])?;
        {
            let mut query = url.query_pairs_mut();
            for segment_type in ALL_SEGMENT_TYPES {
                query.append_pair("includeSegmentTypes", segment_type);
            }
        }
        let response = self
            .get_authenticated(url)
            .await
            .with_context(|| format!("获取媒体分段 {item_id} 失败"))?;

        // MediaSegments 是 Jellyfin 10.10 新增的接口。旧服务器没有该路由时，
        // 保持普通播放，而不是让整个播放请求失败。
        if response.status() == StatusCode::NOT_FOUND {
            debug!(%item_id, "Jellyfin 服务器不支持媒体分段接口");
            return Ok(Vec::new());
        }

        let result: MediaSegmentQueryResult = ensure_success(response)
            .await?
            .json()
            .await
            .context("解析 Jellyfin 媒体分段失败")?;
        Ok(result.items)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn playback_info(
        &self,
        user_id: &str,
        item_id: &str,
        start_ticks: i64,
        media_source_id: Option<&str>,
        audio_index: Option<i32>,
        subtitle_index: Option<i32>,
        max_streaming_bitrate: Option<u64>,
    ) -> Result<PlaybackInfoResponse> {
        let max_streaming_bitrate =
            max_streaming_bitrate.map(|value| value.min(MAX_JELLYFIN_STREAMING_BITRATE as u64));
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
            if let Some(bitrate) = max_streaming_bitrate {
                query.append_pair("maxStreamingBitrate", &bitrate.to_string());
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
                    max_streaming_bitrate,
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
        item_id: &str,
        media_type: Option<&str>,
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
            let kind = if media_type == Some("Audio") {
                "Audio"
            } else {
                "Videos"
            };
            let mut direct = self.endpoint(&[kind, item_id, "stream"])?;
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
            warn!(
                error = %format!("{error:#}"),
                "WebSocket 连接后更新设备能力失败"
            );
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
    max_streaming_bitrate: Option<u64>,
) -> Value {
    let max_streaming_bitrate = max_streaming_bitrate
        .map(|value| value.min(MAX_JELLYFIN_STREAMING_BITRATE as u64) as i64)
        .unwrap_or(MAX_JELLYFIN_STREAMING_BITRATE);

    json!({
        "UserId": user_id,
        "StartTimeTicks": start_ticks,
        "MediaSourceId": media_source_id,
        "AudioStreamIndex": audio_index,
        "SubtitleStreamIndex": subtitle_index,
        "MaxStreamingBitrate": max_streaming_bitrate,
        "EnableDirectPlay": true,
        "EnableDirectStream": true,
        "EnableTranscoding": true,
        "AllowVideoStreamCopy": true,
        "AllowAudioStreamCopy": true,
        "AutoOpenLiveStream": true,
        "DeviceProfile": {
            "Name": "mpv",
            "MaxStreamingBitrate": max_streaming_bitrate,
            "MaxStaticBitrate": max_streaming_bitrate,
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
    fn advertises_only_supported_general_commands() {
        let capabilities = session_capabilities();
        let commands = capabilities["SupportedCommands"]
            .as_array()
            .expect("SupportedCommands 应为数组");
        assert!(
            commands
                .iter()
                .any(|command| command.as_str() == Some("PlayMediaSource"))
        );
        assert!(
            commands
                .iter()
                .all(|command| command.as_str() != Some("PlaybackRate"))
        );
    }

    #[test]
    fn applies_dynamic_max_streaming_bitrate_to_playback_profile() {
        let profile = playback_profile("user-1", 42, None, None, None, Some(1_500_000));
        assert_eq!(profile["MaxStreamingBitrate"], json!(1_500_000));
        assert_eq!(
            profile["DeviceProfile"]["MaxStreamingBitrate"],
            json!(1_500_000)
        );
        assert_eq!(
            profile["DeviceProfile"]["MaxStaticBitrate"],
            json!(1_500_000)
        );
    }

    #[test]
    fn bounds_default_and_requested_streaming_bitrate_to_jellyfin_int32() {
        for profile in [
            playback_profile("user-1", 42, None, None, None, None),
            playback_profile("user-1", 42, None, None, None, Some(u64::MAX)),
        ] {
            assert_eq!(
                profile["MaxStreamingBitrate"],
                json!(MAX_JELLYFIN_STREAMING_BITRATE)
            );
            assert_eq!(
                profile["DeviceProfile"]["MaxStreamingBitrate"],
                json!(MAX_JELLYFIN_STREAMING_BITRATE)
            );
            assert_eq!(
                profile["DeviceProfile"]["MaxStaticBitrate"],
                json!(MAX_JELLYFIN_STREAMING_BITRATE)
            );
        }
    }

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

    #[tokio::test]
    async fn requests_all_native_media_segments_and_accepts_legacy_404() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let responses = [
                (
                    200,
                    r#"{"AccessToken":"token","User":{"Id":"user-1","Name":"Alice"}}"#,
                ),
                (404, ""),
                (
                    200,
                    r#"{"Items":[{"Type":"Intro","StartTicks":100,"EndTicks":200},{"Type":"Outro","StartTicks":900,"EndTicks":1000}]}"#,
                ),
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

        assert!(
            client
                .media_segments("legacy-item")
                .await
                .unwrap()
                .is_empty()
        );
        let segments = client.media_segments("item-1").await.unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].segment_type.as_deref(), Some("Intro"));
        assert_eq!(segments[0].start_ticks, Some(100));
        assert_eq!(segments[1].segment_type.as_deref(), Some("Outro"));

        let requests = server.await.unwrap();
        assert!(
            requests[1]
                .starts_with("GET /MediaSegments/legacy-item?includeSegmentTypes=Intro&includeSegmentTypes=Outro&includeSegmentTypes=Recap&includeSegmentTypes=Preview&includeSegmentTypes=Commercial ")
        );
        assert!(
            requests[2].starts_with("GET /MediaSegments/item-1?includeSegmentTypes=Intro&includeSegmentTypes=Outro&includeSegmentTypes=Recap&includeSegmentTypes=Preview&includeSegmentTypes=Commercial ")
        );
    }

    #[tokio::test]
    async fn requests_trickplay_metadata_and_authenticated_tile_image() {
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
                    r#"{"Id":"item-1","Name":"Movie","MediaType":"Video","Trickplay":{"source-1":{"320":{"Width":320,"Height":180,"TileWidth":10,"TileHeight":10,"ThumbnailCount":123,"Interval":10000}}}}"#,
                ),
                (200, "fake-jpeg-data"),
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
        let trickplay = item.trickplay["source-1"].as_ref().unwrap();
        assert_eq!(trickplay["320"].width, 320);
        assert_eq!(trickplay["320"].interval, 10_000);
        let tile = client
            .trickplay_tile("item-1", "source-1", 320, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tile, b"fake-jpeg-data");

        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /Users/user-1/Items/item-1?fields=Trickplay "));
        assert!(
            requests[2]
                .starts_with("GET /Videos/item-1/Trickplay/320/2.jpg?mediaSourceId=source-1 ")
        );
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("x-emby-token: token")
        );
    }

    #[tokio::test]
    async fn sends_selected_bitrate_in_playback_info_query_and_profile() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _authentication_request = read_request(&mut stream).await;
            write_response(
                &mut stream,
                200,
                r#"{"AccessToken":"token","User":{"Id":"user-1","Name":"Alice"}}"#,
            )
            .await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let playback_request = read_request(&mut stream).await;
            write_response(&mut stream, 200, r#"{"MediaSources":[]}"#).await;
            playback_request
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

        let info = client
            .playback_info("user-1", "item-1", 0, None, None, None, Some(8_000_000))
            .await
            .unwrap();
        assert!(info.media_sources.is_empty());

        let playback_request = server.await.unwrap();
        assert!(playback_request.starts_with(
            "POST /Items/item-1/PlaybackInfo?userId=user-1&startTimeTicks=0&autoOpenLiveStream=true&enableDirectPlay=true&enableDirectStream=true&enableTranscoding=true&maxStreamingBitrate=8000000 "
        ));
        let (_, body) = playback_request.split_once("\r\n\r\n").unwrap();
        let profile: Value = serde_json::from_str(body).unwrap();
        assert_eq!(profile["MaxStreamingBitrate"], json!(8_000_000));
        assert_eq!(
            profile["DeviceProfile"]["MaxStreamingBitrate"],
            json!(8_000_000)
        );
        assert_eq!(
            profile["DeviceProfile"]["MaxStaticBitrate"],
            json!(8_000_000)
        );
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
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}
