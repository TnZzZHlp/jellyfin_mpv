use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, timeout},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    jellyfin::JellyfinClient,
    mpv::{MpvBitrateSelection, MpvEvent, MpvHandle, MpvLaunch, spawn_mpv},
    protocol::{
        MediaSegment, MediaSource, MediaStream, PlayRequest, PlaybackReport, PlaybackStopReport,
        RemoteCommand,
    },
    skip_settings::{SkipSettings, SkipSettingsStore},
    thumbnail::{ThumbnailWorker, select_trickplay},
};

const MEDIA_SEGMENT_TIMEOUT: Duration = Duration::from_secs(5);
const ADAPTIVE_BITRATE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const BUFFERING_TRIGGER: Duration = Duration::from_secs(2);
const BITRATE_SWITCH_COOLDOWN: Duration = Duration::from_secs(15);
#[allow(clippy::duration_suboptimal_units)]
const STABLE_PLAYBACK_BEFORE_RECOVERY: Duration = Duration::from_secs(120);
const DEFAULT_ADAPTIVE_BITRATE: u64 = 4_000_000;
const MIN_ADAPTIVE_BITRATE: u64 = 300_000;
const BITRATE_REDUCTION_PERCENT: u64 = 65;
const TICKS_PER_SECOND: f64 = 10_000_000.0;

#[allow(clippy::cast_precision_loss)]
const fn ticks_to_seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

#[allow(clippy::cast_possible_truncation)]
const fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds.max(0.0) * TICKS_PER_SECOND) as i64
}

pub struct Player {
    client: Arc<JellyfinClient>,
    user_id: String,
    mpv_path: PathBuf,
    mpv_args: Vec<OsString>,
    mpv_events: mpsc::Sender<MpvEvent>,
    skip_settings: Arc<SkipSettingsStore>,
    playback: Option<Playback>,
}

struct Playback {
    mpv: MpvHandle,
    item_id: String,
    item_name: String,
    media_type: Option<String>,
    media_source_id: String,
    play_session_id: String,
    live_stream_id: Option<String>,
    play_method: &'static str,
    audio_stream_index: Option<i32>,
    audio_track_ids: HashMap<i32, i32>,
    subtitle_stream_index: Option<i32>,
    media_streams: Vec<MediaStream>,
    thumbnail: Option<ThumbnailWorker>,
    required_http_headers: HashMap<String, Option<String>>,
    auth_generation: u64,
    source_bitrate: Option<u64>,
    max_streaming_bitrate: Option<u64>,
    position_ticks: i64,
    paused: bool,
    muted: bool,
    volume: f64,
    started: bool,
    startup_started: Instant,
    skip_segments: Vec<SkipSegment>,
    buffering_since: Option<Instant>,
    stable_since: Option<Instant>,
    last_bitrate_switch: Option<Instant>,
}

#[derive(Debug)]
struct SkipSegment {
    kind: String,
    start_ticks: i64,
    end_ticks: i64,
    skipped: bool,
}

#[derive(Debug, PartialEq)]
enum BitrateAction {
    Switch {
        max_streaming_bitrate: Option<u64>,
        recovery: bool,
    },
    Exhausted,
}

impl Playback {
    #[allow(clippy::cast_possible_truncation)]
    fn report(&self) -> PlaybackReport<'_> {
        PlaybackReport {
            item_id: &self.item_id,
            media_source_id: &self.media_source_id,
            play_session_id: &self.play_session_id,
            live_stream_id: self.live_stream_id.as_deref(),
            position_ticks: self.position_ticks.max(0),
            can_seek: true,
            is_paused: self.paused,
            is_muted: self.muted,
            volume_level: self.volume.round().clamp(0.0, 100.0) as i32,
            play_method: self.play_method,
            audio_stream_index: self.audio_stream_index,
            subtitle_stream_index: self.subtitle_stream_index,
        }
    }

    fn stop_report(&self, failed: bool) -> PlaybackStopReport<'_> {
        PlaybackStopReport {
            item_id: &self.item_id,
            media_source_id: &self.media_source_id,
            play_session_id: &self.play_session_id,
            live_stream_id: self.live_stream_id.as_deref(),
            position_ticks: self.position_ticks.max(0),
            failed,
        }
    }
}

impl Player {
    pub const fn new(
        client: Arc<JellyfinClient>,
        user_id: String,
        mpv_path: PathBuf,
        mpv_args: Vec<OsString>,
        mpv_events: mpsc::Sender<MpvEvent>,
        skip_settings: Arc<SkipSettingsStore>,
    ) -> Self {
        Self {
            client,
            user_id,
            mpv_path,
            mpv_args,
            mpv_events,
            skip_settings,
            playback: None,
        }
    }

    pub async fn run(
        &mut self,
        mut commands: mpsc::Receiver<RemoteCommand>,
        mut mpv_events: mpsc::Receiver<MpvEvent>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let mut progress = interval(Duration::from_secs(5));
        progress.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut adaptive_bitrate = interval(ADAPTIVE_BITRATE_CHECK_INTERVAL);
        adaptive_bitrate.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = cancellation.cancelled() => {
                    info!("收到托盘退出请求");
                    break;
                }
                () = &mut shutdown => {
                    info!("收到退出信号");
                    break;
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        warn!("远程命令通道已关闭");
                        break;
                    };
                    if let Err(error) = self.handle_command(command).await {
                        error!(%error, "执行远程播放命令失败");
                    }
                }
                event = mpv_events.recv() => {
                    if let Some(event) = event
                        && let Err(error) = self.handle_mpv_event(event).await
                    {
                        error!(%error, "处理 mpv 事件失败");
                    }
                }
                _ = progress.tick() => {
                    self.report_progress().await;
                }
                _ = adaptive_bitrate.tick() => {
                    if let Err(error) = self.check_adaptive_bitrate().await {
                        warn!(%error, "自适应码率调整失败");
                    }
                }
            }
        }

        self.stop_current(false).await;
        Ok(())
    }

    async fn handle_command(&mut self, command: RemoteCommand) -> Result<()> {
        match command {
            RemoteCommand::Play(request) => self.play(request).await,
            RemoteCommand::Stop => {
                self.stop_current(false).await;
                Ok(())
            }
            RemoteCommand::Pause => {
                let playback = self.active_mut()?;
                playback.mpv.set_pause(true).await?;
                playback.paused = true;
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::Resume => {
                let playback = self.active_mut()?;
                playback.mpv.set_pause(false).await?;
                playback.paused = false;
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::TogglePause => {
                let playback = self.active_mut()?;
                let paused = !playback.paused;
                playback.mpv.set_pause(paused).await?;
                playback.paused = paused;
                self.report_progress().await;
                info!(paused, "播放暂停状态已切换");
                Ok(())
            }
            RemoteCommand::Seek(ticks) => {
                let playback = self.active_mut()?;
                let ticks = ticks.max(0);
                playback.mpv.seek(ticks_to_seconds(ticks)).await?;
                playback.position_ticks = ticks;
                mark_passed_skip_segments(&mut playback.skip_segments, ticks);
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::SetVolume(volume) => {
                let playback = self.active_mut()?;
                let volume = volume.clamp(0.0, 100.0);
                playback.mpv.set_volume(volume).await?;
                playback.volume = volume;
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::VolumeUp => self.adjust_volume(2.0).await,
            RemoteCommand::VolumeDown => self.adjust_volume(-2.0).await,
            RemoteCommand::ToggleFullscreen => self.active_mut()?.mpv.toggle_fullscreen().await,
            RemoteCommand::PlaybackRate(rate) => {
                self.active_mut()?.mpv.set_playback_rate(rate).await
            }
            RemoteCommand::Mute(muted) => {
                let playback = self.active_mut()?;
                playback.mpv.set_mute(muted).await?;
                playback.muted = muted;
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::ToggleMute => {
                let playback = self.active_mut()?;
                playback.mpv.toggle_mute().await?;
                playback.muted = !playback.muted;
                self.report_progress().await;
                Ok(())
            }
            RemoteCommand::SetAudio(index) => self.set_audio(index).await,
            RemoteCommand::SetSubtitle(index) => self.set_subtitle(index).await,
        }
    }

    async fn play(&mut self, request: PlayRequest) -> Result<()> {
        let startup_started = Instant::now();
        let index = request.start_index.unwrap_or(0);
        let item_id = request
            .item_ids
            .get(index)
            .or_else(|| request.item_ids.first())
            .ok_or_else(|| anyhow!("播放命令没有 ItemIds"))?
            .clone();
        let start_ticks = request.start_position_ticks.unwrap_or(0).max(0);

        info!(%item_id, "正在处理 Jellyfin 播放命令");

        let phase_started = Instant::now();
        self.stop_current(false).await;
        let previous_stop_ms = phase_started.elapsed().as_millis();

        let item_request = async {
            let phase_started = Instant::now();
            let item = self.client.item(&self.user_id, &item_id).await?;
            Ok::<_, anyhow::Error>((item, phase_started.elapsed().as_millis()))
        };
        let playback_info_request = async {
            let phase_started = Instant::now();
            let info = self
                .client
                .playback_info(
                    &self.user_id,
                    &item_id,
                    start_ticks,
                    request.media_source_id.as_deref(),
                    request.audio_stream_index,
                    request.subtitle_stream_index,
                    None,
                )
                .await?;
            Ok::<_, anyhow::Error>((info, phase_started.elapsed().as_millis()))
        };
        let media_segments_request = async {
            let phase_started = Instant::now();
            let skip_segments =
                match timeout(MEDIA_SEGMENT_TIMEOUT, self.client.media_segments(&item_id)).await {
                    Ok(Ok(segments)) => prepare_skip_segments(segments, start_ticks),
                    Ok(Err(error)) => {
                        warn!(%error, %item_id, "读取媒体分段失败，继续普通播放");
                        Vec::new()
                    }
                    Err(_) => {
                        warn!(
                            %item_id,
                            timeout_seconds = MEDIA_SEGMENT_TIMEOUT.as_secs(),
                            "读取媒体分段超时，继续普通播放"
                        );
                        Vec::new()
                    }
                };
            Ok::<_, anyhow::Error>((skip_segments, phase_started.elapsed().as_millis()))
        };
        let ((item, item_ms), (info, playback_info_ms), (skip_segments, media_segments_ms)) =
            tokio::try_join!(item_request, playback_info_request, media_segments_request)?;
        info!(
            %item_id,
            previous_stop_ms = %previous_stop_ms,
            item_ms = %item_ms,
            playback_info_ms = %playback_info_ms,
            media_segments_ms = %media_segments_ms,
            elapsed_ms = %startup_started.elapsed().as_millis(),
            "播放启动元数据已准备"
        );
        let source = request
            .media_source_id
            .as_deref()
            .and_then(|wanted| {
                info.media_sources
                    .iter()
                    .find(|source| source.id.as_deref() == Some(wanted))
            })
            .or_else(|| info.media_sources.first())
            .ok_or_else(|| anyhow!("Jellyfin 播放信息中没有媒体源"))?;
        let source_id = source.id.clone().unwrap_or_else(|| item.id.clone());
        let play_session_id = info
            .play_session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let play_method = playback_method(source);
        let url = self.client.stream_url(
            &item.id,
            item.media_type.as_deref(),
            &source_id,
            &play_session_id,
            source.transcoding_url.as_deref(),
        )?;
        let (auth_generation, headers) = self
            .client
            .mpv_http_headers(&source.required_http_headers)
            .await;
        let audio_stream_index = request
            .audio_stream_index
            .or(source.default_audio_stream_index);
        let subtitle_stream_index = request.subtitle_stream_index.map_or_else(
            || {
                source
                    .default_subtitle_stream_index
                    .filter(|index| *index >= 0)
            },
            |index| (index >= 0).then_some(index),
        );
        let subtitle_url = subtitle_stream_index
            .and_then(|index| {
                source
                    .media_streams
                    .iter()
                    .find(|stream| stream.stream_type == "Subtitle" && stream.index == index)
            })
            .map(|stream| self.client.subtitle_url(&item.id, &source_id, stream))
            .transpose()?
            .flatten();
        let media_streams = source.media_streams.clone();
        let trickplay = select_trickplay(&item, &source_id);
        let playback_settings = self.skip_settings.snapshot();
        let phase_started = Instant::now();
        let mpv = match spawn_mpv(MpvLaunch {
            executable: self.mpv_path.clone(),
            extra_args: self.mpv_args.clone(),
            window_title: item.name.clone(),
            bring_to_front_on_start: playback_settings.bring_to_front_on_start,
            playback_mode: playback_settings.playback_mode,
            url,
            headers,
            start_seconds: ticks_to_seconds(start_ticks),
            subtitle_url,
            thumbnail_size: trickplay
                .as_ref()
                .map(super::thumbnail::TrickplayProfile::display_size),
            bitrate_selection: bitrate_menu_selection(playback_settings.auto_adjust_bitrate, None),
            events: self.mpv_events.clone(),
        })
        .await
        {
            Ok(mpv) => mpv,
            Err(error) => return Err(error),
        };
        info!(
            %item_id,
            mpv_ipc_ms = %phase_started.elapsed().as_millis(),
            elapsed_ms = %startup_started.elapsed().as_millis(),
            "mpv 进程、IPC 与加载命令已准备"
        );

        let thumbnail = if let Some(profile) = trickplay {
            match ThumbnailWorker::start(self.client.clone(), mpv.clone(), profile).await {
                Ok(worker) => Some(worker),
                Err(error) => {
                    warn!(%error, %item_id, "初始化 Trickplay 缩略图失败，继续普通播放");
                    None
                }
            }
        } else {
            None
        };

        info!(item = %item.name, item_id = %item.id, method = play_method, "正在启动播放");
        self.playback = Some(Playback {
            mpv,
            item_id: item.id,
            item_name: item.name,
            media_type: item.media_type,
            media_source_id: source_id,
            play_session_id,
            live_stream_id: source.live_stream_id.clone(),
            play_method,
            audio_stream_index,
            audio_track_ids: HashMap::new(),
            subtitle_stream_index,
            media_streams,
            thumbnail,
            required_http_headers: source.required_http_headers.clone(),
            auth_generation,
            source_bitrate: media_source_bitrate(source),
            max_streaming_bitrate: None,
            position_ticks: start_ticks,
            paused: false,
            muted: false,
            volume: 100.0,
            started: false,
            startup_started,
            skip_segments,
            buffering_since: None,
            stable_since: None,
            last_bitrate_switch: None,
        });
        Ok(())
    }

    async fn set_subtitle(&mut self, index: Option<i32>) -> Result<()> {
        let Some(index) = index else {
            let playback = self.active_mut()?;
            playback.mpv.disable_subtitle().await?;
            playback.subtitle_stream_index = None;
            self.report_progress().await;
            info!("字幕已关闭");
            return Ok(());
        };

        let (item_id, source_id, stream) = {
            let playback = self.active_mut()?;
            let stream = playback
                .media_streams
                .iter()
                .find(|stream| stream.stream_type == "Subtitle" && stream.index == index)
                .cloned()
                .ok_or_else(|| anyhow!("找不到字幕流索引 {index}"))?;
            (
                playback.item_id.clone(),
                playback.media_source_id.clone(),
                stream,
            )
        };
        let url = self
            .client
            .subtitle_url(&item_id, &source_id, &stream)?
            .ok_or_else(|| anyhow!("字幕流 {index} 不能作为外部字幕加载"))?;
        let playback = self.active_mut()?;
        playback.mpv.load_subtitle(url).await?;
        playback.subtitle_stream_index = Some(index);
        let title = stream
            .display_title
            .as_deref()
            .or(stream.language.as_deref())
            .unwrap_or("未知字幕");
        info!(index, title, "字幕已加载");
        self.report_progress().await;
        Ok(())
    }

    async fn set_audio(&mut self, index: Option<i32>) -> Result<()> {
        let Some(index) = index else {
            let playback = self.active_mut()?;
            playback.mpv.set_audio_track(None).await?;
            playback.audio_stream_index = None;
            self.report_progress().await;
            info!("音轨已关闭");
            return Ok(());
        };

        let (mpv, track_id, stream) = {
            let playback = self.active_mut()?;
            let stream = playback
                .media_streams
                .iter()
                .find(|stream| stream.stream_type == "Audio" && stream.index == index)
                .cloned()
                .ok_or_else(|| anyhow!("找不到音轨索引 {index}"))?;
            let track_id = playback
                .audio_track_ids
                .get(&index)
                .copied()
                .unwrap_or(index);
            (playback.mpv.clone(), track_id, stream)
        };
        mpv.set_audio_track(Some(track_id)).await?;

        let playback = self.active_mut()?;
        playback.audio_stream_index = Some(index);
        let title = stream
            .display_title
            .as_deref()
            .or(stream.language.as_deref())
            .unwrap_or("未知音轨");
        info!(index, track_id, title, "音轨已切换");
        self.report_progress().await;
        Ok(())
    }

    async fn adjust_volume(&mut self, delta: f64) -> Result<()> {
        let playback = self.active_mut()?;
        let volume = (playback.volume + delta).clamp(0.0, 100.0);
        playback.mpv.set_volume(volume).await?;
        playback.volume = volume;
        self.report_progress().await;
        Ok(())
    }

    async fn handle_mpv_event(&mut self, event: MpvEvent) -> Result<()> {
        let event_id = match &event {
            MpvEvent::FileLoaded { id }
            | MpvEvent::Position { id, .. }
            | MpvEvent::Paused { id, .. }
            | MpvEvent::Buffering { id, .. }
            | MpvEvent::Muted { id, .. }
            | MpvEvent::Volume { id, .. }
            | MpvEvent::AudioTracks { id, .. }
            | MpvEvent::Preview { id, .. }
            | MpvEvent::BitrateSelection { id, .. }
            | MpvEvent::Ended { id, .. }
            | MpvEvent::Terminated { id, .. } => *id,
        };
        if self.playback.as_ref().map(|state| state.mpv.id) != Some(event_id) {
            return Ok(());
        }

        match event {
            MpvEvent::FileLoaded { .. } => {
                let playback = self.playback.as_mut().expect("已核对播放 ID");
                playback.stable_since = Some(Instant::now());
                if !playback.started {
                    playback.started = true;
                    info!(
                        item = %playback.item_name,
                        item_id = %playback.item_id,
                        elapsed_ms = %playback.startup_started.elapsed().as_millis(),
                        "mpv 已加载媒体"
                    );
                    self.client.report_start(&playback.report()).await?;
                    info!(item = %playback.item_name, "播放已开始并上报 Jellyfin");
                }
                self.refresh_mpv_headers().await;
            }
            MpvEvent::Position { seconds, .. } => {
                self.handle_position(seconds).await?;
            }
            MpvEvent::Paused { paused, .. } => {
                let playback = self.playback.as_mut().expect("已核对播放 ID");
                playback.paused = paused;
                if paused {
                    playback.stable_since = None;
                } else if playback.buffering_since.is_none() {
                    playback.stable_since = Some(Instant::now());
                }
            }
            MpvEvent::Buffering { buffering, .. } => {
                let auto_adjust_bitrate = self.skip_settings.snapshot().auto_adjust_bitrate;
                let playback = self.playback.as_mut().expect("已核对播放 ID");
                if !playback.started || playback.paused || !auto_adjust_bitrate {
                    if !auto_adjust_bitrate {
                        playback.buffering_since = None;
                        playback.stable_since = None;
                    }
                    return Ok(());
                }
                if buffering {
                    if playback.buffering_since.is_none() {
                        playback.buffering_since = Some(Instant::now());
                    }
                    playback.stable_since = None;
                } else {
                    playback.buffering_since = None;
                    playback.stable_since = Some(Instant::now());
                }
            }
            MpvEvent::Muted { muted, .. } => {
                self.playback.as_mut().expect("已核对播放 ID").muted = muted;
            }
            MpvEvent::Volume { volume, .. } => {
                if volume.is_finite() {
                    self.playback.as_mut().expect("已核对播放 ID").volume = volume;
                }
            }
            MpvEvent::AudioTracks {
                audio_track_ids, ..
            } => {
                self.playback
                    .as_mut()
                    .expect("已核对播放 ID")
                    .audio_track_ids = audio_track_ids;
            }
            MpvEvent::Preview { request, .. } => {
                if let Some(thumbnail) = self
                    .playback
                    .as_ref()
                    .and_then(|playback| playback.thumbnail.as_ref())
                {
                    thumbnail.request(request);
                }
            }
            MpvEvent::BitrateSelection { selection, .. } => {
                self.select_bitrate(selection).await?;
            }
            MpvEvent::Ended { failed, .. } | MpvEvent::Terminated { failed, .. } => {
                self.stop_current(failed).await;
            }
        }
        Ok(())
    }

    async fn handle_position(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() {
            return Ok(());
        }

        let position_ticks = seconds_to_ticks(seconds);
        let settings = self.skip_settings.snapshot();
        let skip = {
            let playback = self.playback.as_mut().expect("已核对播放 ID");
            if !playback.started {
                return Ok(());
            }
            playback.position_ticks = position_ticks;
            next_skip_target(&mut playback.skip_segments, position_ticks, &settings)
        };

        let Some((kind, target_ticks)) = skip else {
            return Ok(());
        };
        let playback = self.playback.as_mut().expect("播放状态仍然存在");
        playback.mpv.seek(ticks_to_seconds(target_ticks)).await?;
        playback.position_ticks = target_ticks;
        info!(
            segment_type = %kind,
            target_seconds = ticks_to_seconds(target_ticks),
            "已自动跳过 Jellyfin 媒体分段"
        );
        self.report_progress().await;
        Ok(())
    }

    async fn select_bitrate(&mut self, selection: MpvBitrateSelection) -> Result<()> {
        let max_streaming_bitrate = selection.max_streaming_bitrate();
        let previous_settings = self.skip_settings.snapshot();
        let (previous_selection, mpv, needs_switch) = self.playback.as_ref().map_or_else(
            || {
                (
                    bitrate_menu_selection(previous_settings.auto_adjust_bitrate, None),
                    None,
                    false,
                )
            },
            |playback| {
                (
                    bitrate_menu_selection(
                        previous_settings.auto_adjust_bitrate,
                        playback.max_streaming_bitrate,
                    ),
                    Some(playback.mpv.clone()),
                    playback.max_streaming_bitrate != max_streaming_bitrate,
                )
            },
        );
        let settings_changed =
            previous_settings.auto_adjust_bitrate != selection.enables_auto_adjustment();
        if settings_changed {
            let mut settings = previous_settings.clone();
            settings.auto_adjust_bitrate = selection.enables_auto_adjustment();
            if let Err(error) = self.skip_settings.update(settings) {
                if let Some(mpv) = mpv.as_ref()
                    && let Err(restore_error) = mpv.set_bitrate_selection(previous_selection).await
                {
                    warn!(%restore_error, "恢复 mpv 码率菜单状态失败");
                }
                return Err(error);
            }
        }

        let result = if needs_switch {
            self.switch_bitrate(max_streaming_bitrate).await
        } else {
            Ok(true)
        };
        match result {
            Ok(true) => {
                let now = Instant::now();
                if let Some(playback) = self.playback.as_mut() {
                    playback.buffering_since = None;
                    playback.stable_since = Some(now);
                    playback.last_bitrate_switch = Some(now);
                }
                if let Some(mpv) = mpv
                    && let Err(error) = mpv.set_bitrate_selection(selection).await
                {
                    warn!(%error, "更新 mpv 码率菜单状态失败");
                }
                info!(quality = %selection.label(), "已从 mpv 码率菜单切换播放质量");
                Ok(())
            }
            Ok(false) => {
                self.restore_bitrate_menu_selection(
                    previous_settings,
                    settings_changed,
                    previous_selection,
                    mpv,
                )
                .await;
                warn!(quality = %selection.label(), "当前媒体无法切换码率");
                Ok(())
            }
            Err(error) => {
                self.restore_bitrate_menu_selection(
                    previous_settings,
                    settings_changed,
                    previous_selection,
                    mpv,
                )
                .await;
                Err(error)
            }
        }
    }

    async fn restore_bitrate_menu_selection(
        &self,
        previous_settings: SkipSettings,
        settings_changed: bool,
        selection: MpvBitrateSelection,
        mpv: Option<MpvHandle>,
    ) {
        if settings_changed && let Err(error) = self.skip_settings.update(previous_settings) {
            warn!(%error, "恢复自动调整码率设置失败");
        }
        if let Some(mpv) = mpv
            && let Err(error) = mpv.set_bitrate_selection(selection).await
        {
            warn!(%error, "恢复 mpv 码率菜单状态失败");
        }
    }

    async fn check_adaptive_bitrate(&mut self) -> Result<()> {
        if !self.skip_settings.snapshot().auto_adjust_bitrate {
            if let Some(playback) = self.playback.as_mut() {
                playback.buffering_since = None;
                playback.stable_since = None;
            }
            return Ok(());
        }

        let now = Instant::now();
        let action =
            {
                let Some(playback) = self.playback.as_ref() else {
                    return Ok(());
                };
                if !playback.started || playback.paused {
                    return Ok(());
                }

                let cooldown_elapsed = playback
                    .last_bitrate_switch
                    .is_none_or(|last| now.duration_since(last) >= BITRATE_SWITCH_COOLDOWN);
                if !cooldown_elapsed {
                    return Ok(());
                }

                if playback
                    .buffering_since
                    .is_some_and(|since| now.duration_since(since) >= BUFFERING_TRIGGER)
                {
                    next_lower_bitrate(playback.max_streaming_bitrate, playback.source_bitrate)
                        .map_or(Some(BitrateAction::Exhausted), |max_streaming_bitrate| {
                            Some(BitrateAction::Switch {
                                max_streaming_bitrate: Some(max_streaming_bitrate),
                                recovery: false,
                            })
                        })
                } else if playback.buffering_since.is_none()
                    && playback.max_streaming_bitrate.is_some()
                    && playback.stable_since.is_some_and(|since| {
                        now.duration_since(since) >= STABLE_PLAYBACK_BEFORE_RECOVERY
                    })
                {
                    next_higher_bitrate(playback.max_streaming_bitrate, playback.source_bitrate)
                } else {
                    None
                }
            };

        match action {
            Some(BitrateAction::Switch {
                max_streaming_bitrate,
                recovery,
            }) => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.last_bitrate_switch = Some(now);
                }
                let switched = self.switch_bitrate(max_streaming_bitrate).await?;
                if let Some(playback) = self.playback.as_mut() {
                    if switched {
                        if recovery {
                            if let Some(value) = max_streaming_bitrate {
                                info!(bitrate_bps = value, "稳定播放后恢复较高码率");
                            } else {
                                info!("稳定播放后恢复原始最高码率");
                            }
                            playback.stable_since = Some(now);
                        } else {
                            if let Some(value) = max_streaming_bitrate {
                                info!(bitrate_bps = value, "检测到持续缓冲，已降低码率");
                            }
                            playback.buffering_since = Some(now);
                            playback.stable_since = None;
                        }
                    } else if recovery {
                        playback.stable_since = None;
                    } else {
                        playback.buffering_since = None;
                    }
                }
            }
            Some(BitrateAction::Exhausted) => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.buffering_since = None;
                }
                info!("已达到自适应码率下限");
            }
            None => {}
        }
        Ok(())
    }

    async fn switch_bitrate(&mut self, max_streaming_bitrate: Option<u64>) -> Result<bool> {
        let (
            item_id,
            media_type,
            current_source_id,
            audio_stream_index,
            subtitle_stream_index,
            position_ticks,
            source_bitrate,
            mpv,
        ) = {
            let Some(playback) = self.playback.as_ref() else {
                return Ok(false);
            };
            if !playback.started || playback.media_type.as_deref() != Some("Video") {
                return Ok(false);
            }
            (
                playback.item_id.clone(),
                playback.media_type.clone(),
                playback.media_source_id.clone(),
                playback.audio_stream_index,
                playback.subtitle_stream_index,
                playback.position_ticks.max(0),
                playback.source_bitrate,
                playback.mpv.clone(),
            )
        };

        let info = self
            .client
            .playback_info(
                &self.user_id,
                &item_id,
                position_ticks,
                Some(&current_source_id),
                audio_stream_index,
                subtitle_stream_index,
                max_streaming_bitrate,
            )
            .await?;
        let source = info
            .media_sources
            .iter()
            .find(|source| source.id.as_deref() == Some(&current_source_id))
            .or_else(|| info.media_sources.first())
            .ok_or_else(|| anyhow!("Jellyfin 降码率请求没有返回媒体源"))?;
        let source_id = source
            .id
            .clone()
            .unwrap_or_else(|| current_source_id.clone());
        let play_session_id = info
            .play_session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let url = self.client.stream_url(
            &item_id,
            media_type.as_deref(),
            &source_id,
            &play_session_id,
            source.transcoding_url.as_deref(),
        )?;
        let subtitle_url = subtitle_stream_index
            .and_then(|index| {
                source
                    .media_streams
                    .iter()
                    .find(|stream| stream.stream_type == "Subtitle" && stream.index == index)
            })
            .map(|stream| self.client.subtitle_url(&item_id, &source_id, stream))
            .transpose()?
            .flatten();
        let (auth_generation, headers) = self
            .client
            .mpv_http_headers(&source.required_http_headers)
            .await;
        mpv.set_http_headers(headers).await?;
        mpv.load_file(url, ticks_to_seconds(position_ticks), subtitle_url)
            .await?;

        let new_source_bitrate = media_source_bitrate(source);
        let media_streams = source.media_streams.clone();
        let play_method = playback_method(source);
        let required_http_headers = source.required_http_headers.clone();
        let live_stream_id = source.live_stream_id.clone();
        let playback = self.playback.as_mut().expect("播放状态仍然存在");
        playback.media_source_id = source_id;
        playback.play_session_id = play_session_id;
        playback.live_stream_id = live_stream_id;
        playback.play_method = play_method;
        playback.media_streams = media_streams;
        playback.audio_track_ids.clear();
        playback.required_http_headers = required_http_headers;
        playback.auth_generation = auth_generation;
        playback.source_bitrate = source_bitrate.or(new_source_bitrate);
        playback.max_streaming_bitrate = max_streaming_bitrate;

        self.report_progress().await;
        Ok(true)
    }

    async fn report_progress(&mut self) {
        let Some(playback) = &self.playback else {
            return;
        };
        if !playback.started {
            return;
        }
        if let Err(error) = self.client.report_progress(&playback.report()).await {
            warn!(%error, "上报 Jellyfin 播放进度失败");
            return;
        }
        self.refresh_mpv_headers().await;
    }

    async fn refresh_mpv_headers(&mut self) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let current_generation = playback.auth_generation;
        let required = playback.required_http_headers.clone();
        let (generation, headers) = self.client.mpv_http_headers(&required).await;
        if generation == current_generation {
            return;
        }

        let playback = self.playback.as_mut().expect("播放状态仍然存在");
        if let Err(error) = playback.mpv.set_http_headers(headers).await {
            warn!(%error, "更新 mpv Access Token 请求头失败");
            return;
        }
        playback.auth_generation = generation;
        info!("mpv Access Token 请求头已更新");
    }

    async fn stop_current(&mut self, failed: bool) {
        let Some(mut playback) = self.playback.take() else {
            return;
        };
        if let Some(thumbnail) = playback.thumbnail.take() {
            thumbnail.stop().await;
        }
        let _ = playback.mpv.quit().await;
        if playback.started {
            if let Err(error) = self.client.report_stop(&playback.stop_report(failed)).await {
                warn!(%error, "上报 Jellyfin 停止播放失败");
            } else {
                info!(item = %playback.item_name, "停止播放已上报 Jellyfin");
            }
        }
    }

    fn active_mut(&mut self) -> Result<&mut Playback> {
        let playback = self
            .playback
            .as_mut()
            .ok_or_else(|| anyhow!("当前没有正在播放的媒体"))?;
        if !playback.started {
            bail!("mpv 尚未完成媒体加载");
        }
        Ok(playback)
    }
}

const fn playback_method(source: &MediaSource) -> &'static str {
    if source.transcoding_url.is_some() {
        "Transcode"
    } else if source.supports_direct_play {
        "DirectPlay"
    } else {
        "DirectStream"
    }
}

fn media_source_bitrate(source: &MediaSource) -> Option<u64> {
    source
        .bitrate
        .and_then(|bitrate| u64::try_from(bitrate).ok())
        .filter(|bitrate| *bitrate > 0)
}

fn bitrate_menu_selection(
    auto_adjust_bitrate: bool,
    max_streaming_bitrate: Option<u64>,
) -> MpvBitrateSelection {
    if auto_adjust_bitrate {
        MpvBitrateSelection::Adaptive
    } else {
        max_streaming_bitrate.map_or(MpvBitrateSelection::Auto, MpvBitrateSelection::Limit)
    }
}

fn next_lower_bitrate(current: Option<u64>, source_bitrate: Option<u64>) -> Option<u64> {
    let baseline = current
        .or(source_bitrate)
        .unwrap_or(DEFAULT_ADAPTIVE_BITRATE);
    let reduced = baseline
        .saturating_mul(BITRATE_REDUCTION_PERCENT)
        .saturating_div(100)
        .max(MIN_ADAPTIVE_BITRATE);
    let next = reduced.min(baseline.saturating_sub(1));
    (next >= MIN_ADAPTIVE_BITRATE && next < baseline).then_some(next)
}

fn next_higher_bitrate(current: Option<u64>, source_bitrate: Option<u64>) -> Option<BitrateAction> {
    let current = current?;
    let ceiling = source_bitrate.unwrap_or(DEFAULT_ADAPTIVE_BITRATE);
    if current >= ceiling {
        return Some(BitrateAction::Switch {
            max_streaming_bitrate: None,
            recovery: true,
        });
    }

    let increased = current
        .saturating_mul(100)
        .saturating_div(BITRATE_REDUCTION_PERCENT);
    if increased >= ceiling {
        Some(BitrateAction::Switch {
            max_streaming_bitrate: None,
            recovery: true,
        })
    } else if increased > current {
        Some(BitrateAction::Switch {
            max_streaming_bitrate: Some(increased),
            recovery: true,
        })
    } else {
        None
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("无法监听 SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn prepare_skip_segments(segments: Vec<MediaSegment>, initial_ticks: i64) -> Vec<SkipSegment> {
    let mut segments = segments
        .into_iter()
        .filter_map(|segment| {
            let kind = segment.segment_type?;
            if !SkipSettings::supports(&kind) {
                return None;
            }
            let start_ticks = segment.start_ticks?.max(0);
            let end_ticks = segment.end_ticks?;
            (end_ticks > start_ticks).then_some(SkipSegment {
                kind,
                start_ticks,
                end_ticks,
                skipped: end_ticks <= initial_ticks,
            })
        })
        .collect::<Vec<_>>();
    segments.sort_unstable_by_key(|segment| segment.start_ticks);
    segments
}

fn next_skip_target(
    segments: &mut [SkipSegment],
    position_ticks: i64,
    settings: &SkipSettings,
) -> Option<(String, i64)> {
    // 如果用户直接拖到了某个分段之后，就把它视为已处理；之后手动回看时
    // 不应再次把用户强制送到分段末尾。
    mark_passed_skip_segments(segments, position_ticks);
    let segment = segments.iter_mut().find(|segment| {
        !segment.skipped
            && settings.is_enabled(&segment.kind)
            && position_ticks >= segment.start_ticks
            && position_ticks < segment.end_ticks
    })?;
    segment.skipped = true;
    Some((segment.kind.clone(), segment.end_ticks))
}

fn mark_passed_skip_segments(segments: &mut [SkipSegment], position_ticks: i64) {
    for segment in segments.iter_mut() {
        if segment.end_ticks <= position_ticks {
            segment.skipped = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_and_skips_intro_and_outro_once() {
        let settings = SkipSettings::default();
        let mut segments = prepare_skip_segments(
            vec![
                media_segment("Outro", 900, 1_000),
                media_segment("Commercial", 400, 500),
                media_segment("Intro", 100, 200),
                media_segment("Intro", 300, 300),
            ],
            0,
        );

        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].kind, "Intro");
        assert_eq!(segments[1].kind, "Commercial");
        assert_eq!(segments[2].kind, "Outro");
        assert_eq!(
            next_skip_target(&mut segments, 150, &settings),
            Some(("Intro".to_owned(), 200))
        );
        assert_eq!(next_skip_target(&mut segments, 150, &settings), None);
        assert_eq!(next_skip_target(&mut segments, 450, &settings), None);
        assert_eq!(
            next_skip_target(&mut segments, 950, &settings),
            Some(("Outro".to_owned(), 1_000))
        );
    }

    #[test]
    fn does_not_auto_skip_segments_already_passed_at_resume_or_seek() {
        let settings = SkipSettings::default();
        let mut resumed = prepare_skip_segments(vec![media_segment("Intro", 100, 200)], 250);
        assert_eq!(next_skip_target(&mut resumed, 150, &settings), None);

        let mut sought_past = prepare_skip_segments(vec![media_segment("Intro", 100, 200)], 0);
        assert_eq!(next_skip_target(&mut sought_past, 250, &settings), None);
        assert_eq!(next_skip_target(&mut sought_past, 150, &settings), None);
    }

    #[test]
    fn uses_updated_skip_type_choices() {
        let mut segments = prepare_skip_segments(
            vec![
                media_segment("Recap", 100, 200),
                media_segment("Commercial", 300, 400),
            ],
            0,
        );
        let mut settings = SkipSettings::default();
        assert_eq!(next_skip_target(&mut segments, 150, &settings), None);

        settings.recap = true;
        assert_eq!(
            next_skip_target(&mut segments, 150, &settings),
            Some(("Recap".to_owned(), 200))
        );
    }

    #[test]
    fn derives_f9_menu_selection_from_current_bitrate_mode() {
        assert_eq!(
            bitrate_menu_selection(true, Some(4_000_000)),
            MpvBitrateSelection::Adaptive
        );
        assert_eq!(
            bitrate_menu_selection(false, None),
            MpvBitrateSelection::Auto
        );
        assert_eq!(
            bitrate_menu_selection(false, Some(4_000_000)),
            MpvBitrateSelection::Limit(4_000_000)
        );
    }

    #[test]
    fn lowers_bitrate_in_steps_and_recovers_to_the_original_ceiling() {
        assert_eq!(next_lower_bitrate(None, Some(10_000_000)), Some(6_500_000));
        assert_eq!(
            next_lower_bitrate(Some(6_500_000), Some(10_000_000)),
            Some(4_225_000)
        );
        assert_eq!(
            next_higher_bitrate(Some(4_225_000), Some(10_000_000)),
            Some(BitrateAction::Switch {
                max_streaming_bitrate: Some(6_500_000),
                recovery: true,
            })
        );
        assert_eq!(
            next_higher_bitrate(Some(6_500_000), Some(10_000_000)),
            Some(BitrateAction::Switch {
                max_streaming_bitrate: None,
                recovery: true,
            })
        );
    }

    #[test]
    fn does_not_reduce_below_the_adaptive_floor() {
        assert_eq!(
            next_lower_bitrate(Some(MIN_ADAPTIVE_BITRATE), Some(1_000_000)),
            None
        );
        assert_eq!(
            next_lower_bitrate(Some(400_000), Some(1_000_000)),
            Some(300_000)
        );
    }

    fn media_segment(kind: &str, start_ticks: i64, end_ticks: i64) -> MediaSegment {
        MediaSegment {
            segment_type: Some(kind.to_owned()),
            start_ticks: Some(start_ticks),
            end_ticks: Some(end_ticks),
        }
    }
}
