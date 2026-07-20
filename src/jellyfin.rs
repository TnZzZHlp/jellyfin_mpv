use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Instant, interval_at, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    protocol::{
        AuthenticationResult, ItemDto, PlaybackInfoResponse, PlaybackReport, PlaybackStopReport,
        RemoteCommand, SocketAction, UserDto, parse_socket_message,
    },
};

const CLIENT_NAME: &str = "Jellyfin MPV Cast";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct JellyfinClient {
    config: Config,
    http: Client,
}

impl JellyfinClient {
    pub async fn connect(mut config: Config) -> Result<(Self, UserDto)> {
        let user = if config.token.is_some() {
            let client = Self::from_authenticated_config(config.clone())?;
            client.current_user().await?
        } else {
            let result = authenticate_by_name(&config).await?;
            config.token = Some(result.access_token);
            result.user
        };
        // 登录成功后不再保留密码。
        config.password = None;
        let client = Self::from_authenticated_config(config)?;
        Ok((client, user))
    }

    fn from_authenticated_config(config: Config) -> Result<Self> {
        let http = Client::builder()
            .default_headers(auth_headers(&config, Some(config.token()))?)
            .timeout(Duration::from_secs(30))
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self { config, http })
    }

    pub async fn current_user(&self) -> Result<UserDto> {
        let response = self
            .http
            .get(self.endpoint(&["Users", "Me"])?)
            .send()
            .await
            .context("请求 Jellyfin 当前用户失败")?;
        if response.status() == StatusCode::BAD_REQUEST {
            bail!(
                "这个 Token 不属于任何 Jellyfin 用户；请使用用户登录返回的 AccessToken，不能使用控制台“API 密钥”"
            );
        }
        ensure_success(response)
            .await?
            .json()
            .await
            .context("解析 Jellyfin 当前用户失败")
    }

    pub async fn register_capabilities(&self) -> Result<()> {
        let body = json!({
            "PlayableMediaTypes": ["Video", "Audio"],
            "SupportedCommands": ["SetVolume", "Mute", "Unmute", "ToggleMute"],
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
            .http
            .post(url)
            .json(&playback_profile(
                user_id,
                start_ticks,
                media_source_id,
                audio_index,
                subtitle_index,
            ))
            .send()
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
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| !key.eq_ignore_ascii_case("api_key"))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            let mut query = url.query_pairs_mut();
            query.extend_pairs(pairs);
        }
        Ok(url)
    }

    pub fn mpv_http_headers(
        &self,
        required: &std::collections::HashMap<String, Option<String>>,
    ) -> Vec<String> {
        let mut headers = vec![format!("X-Emby-Token: {}", self.config.token())];
        headers.extend(
            required
                .iter()
                .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}: {value}"))),
        );
        headers
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
        let mut url = self.config.server.clone();
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|()| anyhow!("无法构造 WebSocket 地址"))?;
        url = url.join("socket").context("无法构造 WebSocket 地址")?;
        url.query_pairs_mut()
            .append_pair("api_key", self.config.token())
            .append_pair("deviceId", self.config.device_id());

        let (mut socket, _) = connect_async(url.as_str())
            .await
            .context("WebSocket 握手失败")?;
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
        let response = self.http.get(url).send().await.context("HTTP 请求失败")?;
        ensure_success(response)
            .await?
            .json()
            .await
            .context("解析 Jellyfin JSON 响应失败")
    }

    async fn post_json<T: Serialize + ?Sized>(&self, url: Url, body: &T) -> Result<()> {
        let response = self
            .http
            .post(url)
            .json(body)
            .send()
            .await
            .context("HTTP 请求失败")?;
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
        bail!("Jellyfin 用户名或密码错误");
    }
    ensure_success(response)
        .await?
        .json()
        .await
        .context("解析 Jellyfin 登录响应失败")
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
    headers.insert(
        "X-Emby-Authorization",
        HeaderValue::from_str(&auth).context("用户名、Token 或设备名称含有非法 HTTP 字符")?,
    );
    if let Some(token) = token {
        headers.insert(
            "X-Emby-Token",
            HeaderValue::from_str(token).context("Access Token 含有非法 HTTP 字符")?,
        );
    }
    Ok(headers)
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
