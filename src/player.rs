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
    jellyfin::{JellyfinClient, PlaybackConstraints},
    mpv::{MpvEndReason, MpvEvent, MpvHandle, MpvLaunch, MpvQualitySelection, spawn_mpv},
    protocol::{
        MediaSegment, MediaStream, PlayCommand, PlayRequest, PlaybackOrder, PlaybackQueueItem,
        PlaybackReport, PlaybackStopReport, RemoteCommand, RepeatMode,
    },
    skip_settings::{SkipSettings, SkipSettingsStore},
    thumbnail::{ThumbnailWorker, TrickplayProfile, select_trickplay},
};
use url::Url;

const MEDIA_SEGMENT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Player {
    client: Arc<JellyfinClient>,
    user_id: String,
    mpv_path: PathBuf,
    mpv_args: Vec<OsString>,
    mpv_events: mpsc::Sender<MpvEvent>,
    skip_settings: Arc<SkipSettingsStore>,
    playback: Option<Playback>,
    queue: Option<PlaybackQueue>,
    repeat_mode: RepeatMode,
    playback_order: PlaybackOrder,
    playback_constraints: PlaybackConstraints,
}

struct Playback {
    mpv: MpvHandle,
    item_id: String,
    playlist_item_id: String,
    item_name: String,
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
    position_ticks: i64,
    paused: bool,
    muted: bool,
    volume: f64,
    playback_rate: f64,
    playback_constraints: PlaybackConstraints,
    started: bool,
    skip_segments: Vec<SkipSegment>,
    last_position_update: Instant,
}

struct PreparedPlayback {
    item_id: String,
    playlist_item_id: String,
    item_name: String,
    media_source_id: String,
    play_session_id: String,
    live_stream_id: Option<String>,
    play_method: &'static str,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
    media_streams: Vec<MediaStream>,
    required_http_headers: HashMap<String, Option<String>>,
    auth_generation: u64,
    url: Url,
    headers: Vec<String>,
    subtitle_url: Option<Url>,
    thumbnail_profile: Option<TrickplayProfile>,
    start_ticks: i64,
    skip_segments: Vec<SkipSegment>,
}

#[derive(Debug, Clone, Copy)]
struct PlaybackPreferences {
    paused: bool,
    muted: bool,
    volume: f64,
    playback_rate: f64,
}

impl Default for PlaybackPreferences {
    fn default() -> Self {
        Self {
            paused: false,
            muted: false,
            volume: 100.0,
            playback_rate: 1.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct StartOptions {
    start_ticks: i64,
    media_source_id: Option<String>,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
    preferences: PlaybackPreferences,
}

#[derive(Debug, Clone)]
struct PlaybackQueue {
    items: Vec<PlaybackQueueItem>,
    sorted_items: Vec<PlaybackQueueItem>,
    current_index: usize,
}

impl PlaybackQueue {
    fn new(item_ids: Vec<String>, start_index: usize, shuffled: bool) -> Result<Self> {
        if item_ids.is_empty() {
            bail!("播放命令没有 ItemIds");
        }

        let sorted_items = item_ids
            .into_iter()
            .map(|id| PlaybackQueueItem {
                id,
                playlist_item_id: Uuid::new_v4().to_string(),
            })
            .collect::<Vec<_>>();
        let mut items = sorted_items.clone();
        let current_index = if shuffled {
            shuffle_items(&mut items);
            0
        } else {
            if start_index < items.len() {
                start_index
            } else {
                0
            }
        };

        Ok(Self {
            items,
            sorted_items,
            current_index,
        })
    }

    fn current(&self) -> Option<&PlaybackQueueItem> {
        self.items.get(self.current_index)
    }

    fn next_index(&self, repeat_mode: RepeatMode) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if self.current_index + 1 < self.items.len() {
            return Some(self.current_index + 1);
        }
        match repeat_mode {
            RepeatMode::RepeatAll => Some(0),
            RepeatMode::RepeatOne => Some(self.current_index),
            RepeatMode::RepeatNone => None,
        }
    }

    fn previous_index(&self, repeat_mode: RepeatMode) -> Option<usize> {
        if self.current_index > 0 {
            return Some(self.current_index - 1);
        }
        match repeat_mode {
            RepeatMode::RepeatAll => self.items.len().checked_sub(1),
            RepeatMode::RepeatNone | RepeatMode::RepeatOne => None,
        }
    }

    fn set_current(&mut self, index: usize) {
        if !self.items.is_empty() {
            self.current_index = index.min(self.items.len() - 1);
        }
    }

    fn insert_next(&mut self, item_ids: Vec<String>) {
        let entries = make_queue_items(item_ids);
        if entries.is_empty() {
            return;
        }
        let insertion = (self.current_index + 1).min(self.items.len());
        self.items.splice(insertion..insertion, entries.clone());
        self.sorted_items.extend(entries);
    }

    fn append(&mut self, item_ids: Vec<String>) {
        let entries = make_queue_items(item_ids);
        if entries.is_empty() {
            return;
        }
        self.items.extend(entries.clone());
        self.sorted_items.extend(entries);
    }

    fn set_order(&mut self, order: PlaybackOrder) {
        let Some(current) = self.current().cloned() else {
            return;
        };

        match order {
            PlaybackOrder::Default => {
                self.items = self.sorted_items.clone();
                self.current_index = self
                    .items
                    .iter()
                    .position(|item| item.playlist_item_id == current.playlist_item_id)
                    .unwrap_or(0);
            }
            PlaybackOrder::Shuffle => {
                let mut remaining = self
                    .items
                    .iter()
                    .filter(|item| item.playlist_item_id != current.playlist_item_id)
                    .cloned()
                    .collect::<Vec<_>>();
                shuffle_items(&mut remaining);
                self.items = Vec::with_capacity(remaining.len() + 1);
                self.items.push(current);
                self.items.extend(remaining);
                self.current_index = 0;
            }
        }
    }
}

fn make_queue_items(item_ids: Vec<String>) -> Vec<PlaybackQueueItem> {
    item_ids
        .into_iter()
        .map(|id| PlaybackQueueItem {
            id,
            playlist_item_id: Uuid::new_v4().to_string(),
        })
        .collect()
}

fn shuffle_items<T>(items: &mut [T]) {
    for index in (1..items.len()).rev() {
        let random = Uuid::new_v4().as_u128() as usize;
        items.swap(index, random % (index + 1));
    }
}

#[derive(Debug)]
struct SkipSegment {
    kind: String,
    start_ticks: i64,
    end_ticks: i64,
    skipped: bool,
}

impl Playback {
    fn report<'a>(
        &'a self,
        queue: &'a PlaybackQueue,
        repeat_mode: RepeatMode,
        playback_order: PlaybackOrder,
    ) -> PlaybackReport<'a> {
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
            repeat_mode,
            playback_order,
            now_playing_queue: Some(&queue.items),
            playlist_item_id: Some(&self.playlist_item_id),
        }
    }

    fn stop_report<'a>(
        &'a self,
        failed: bool,
        queue: &'a [PlaybackQueueItem],
    ) -> PlaybackStopReport<'a> {
        PlaybackStopReport {
            item_id: &self.item_id,
            media_source_id: &self.media_source_id,
            play_session_id: &self.play_session_id,
            live_stream_id: self.live_stream_id.as_deref(),
            position_ticks: self.position_ticks.max(0),
            failed,
            now_playing_queue: Some(queue),
            playlist_item_id: queue
                .iter()
                .any(|item| item.playlist_item_id == self.playlist_item_id)
                .then_some(self.playlist_item_id.as_str()),
        }
    }

    fn estimated_position_ticks(&self) -> i64 {
        if self.paused || !self.playback_rate.is_finite() || self.playback_rate <= 0.0 {
            return self.position_ticks.max(0);
        }

        let elapsed_ticks = (self.last_position_update.elapsed().as_secs_f64()
            * self.playback_rate
            * 10_000_000.0) as i64;
        self.position_ticks.saturating_add(elapsed_ticks).max(0)
    }
}

fn playback_from_prepared(
    mpv: MpvHandle,
    thumbnail: Option<ThumbnailWorker>,
    prepared: PreparedPlayback,
    preferences: PlaybackPreferences,
    playback_constraints: PlaybackConstraints,
) -> Playback {
    let PreparedPlayback {
        item_id,
        playlist_item_id,
        item_name,
        media_source_id,
        play_session_id,
        live_stream_id,
        play_method,
        audio_stream_index,
        subtitle_stream_index,
        media_streams,
        required_http_headers,
        auth_generation,
        url: _,
        headers: _,
        subtitle_url: _,
        thumbnail_profile: _,
        start_ticks,
        skip_segments,
    } = prepared;

    Playback {
        mpv,
        item_id,
        playlist_item_id,
        item_name,
        media_source_id,
        play_session_id,
        live_stream_id,
        play_method,
        audio_stream_index,
        audio_track_ids: HashMap::new(),
        subtitle_stream_index,
        media_streams,
        thumbnail,
        required_http_headers,
        auth_generation,
        position_ticks: start_ticks,
        paused: preferences.paused,
        muted: preferences.muted,
        volume: preferences.volume,
        playback_rate: preferences.playback_rate,
        playback_constraints,
        started: false,
        skip_segments,
        last_position_update: Instant::now(),
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
            queue: None,
            repeat_mode: RepeatMode::RepeatNone,
            playback_order: PlaybackOrder::Default,
            playback_constraints: PlaybackConstraints {
                max_streaming_bitrate: None,
                max_width: None,
                max_height: None,
            },
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
            }
        }

        self.queue = None;
        self.stop_current(false).await;
        Ok(())
    }

    async fn handle_command(&mut self, command: RemoteCommand) -> Result<()> {
        match command {
            RemoteCommand::Play(request) => self.play(request).await,
            RemoteCommand::Stop => {
                self.queue = None;
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
            RemoteCommand::NextTrack => self.move_queue(true).await,
            RemoteCommand::PreviousTrack => self.move_queue(false).await,
            RemoteCommand::Seek(ticks) => {
                let playback = self.active_mut()?;
                let ticks = ticks.max(0);
                playback.mpv.seek(ticks as f64 / 10_000_000.0).await?;
                playback.position_ticks = ticks;
                playback.last_position_update = Instant::now();
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
                let playback = self.active_mut()?;
                playback.mpv.set_playback_rate(rate).await?;
                playback.playback_rate = rate;
                Ok(())
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
            RemoteCommand::SetRepeatMode(mode) => self.set_repeat_mode(mode).await,
            RemoteCommand::SetShuffleMode(order) => self.set_playback_order(order).await,
            RemoteCommand::SetMaxStreamingBitrate(bitrate) => {
                self.set_max_streaming_bitrate(bitrate).await
            }
        }
    }

    async fn play(&mut self, request: PlayRequest) -> Result<()> {
        if request.item_ids.is_empty() {
            bail!("播放命令没有 ItemIds");
        }

        let start_ticks = request.start_position_ticks.unwrap_or(0).max(0);
        match request.play_command {
            PlayCommand::PlayNow | PlayCommand::PlayInstantMix | PlayCommand::PlayShuffle => {
                if request.play_command == PlayCommand::PlayShuffle {
                    self.playback_order = PlaybackOrder::Shuffle;
                }
                let shuffled = self.playback_order == PlaybackOrder::Shuffle;
                self.queue = Some(PlaybackQueue::new(
                    request.item_ids,
                    request.start_index.unwrap_or(0),
                    shuffled,
                )?);
                self.stop_current(false).await;
                self.start_current(StartOptions {
                    start_ticks,
                    media_source_id: request.media_source_id,
                    audio_stream_index: request.audio_stream_index,
                    subtitle_stream_index: request.subtitle_stream_index,
                    preferences: PlaybackPreferences::default(),
                })
                .await
            }
            PlayCommand::PlayNext => {
                if self.playback.is_some() && self.queue.is_some() {
                    self.queue
                        .as_mut()
                        .expect("播放存在时队列应存在")
                        .insert_next(request.item_ids);
                    self.report_progress().await;
                    Ok(())
                } else {
                    self.queue = Some(PlaybackQueue::new(
                        request.item_ids,
                        request.start_index.unwrap_or(0),
                        self.playback_order == PlaybackOrder::Shuffle,
                    )?);
                    self.stop_current(false).await;
                    self.start_current(StartOptions {
                        start_ticks,
                        media_source_id: request.media_source_id,
                        audio_stream_index: request.audio_stream_index,
                        subtitle_stream_index: request.subtitle_stream_index,
                        preferences: PlaybackPreferences::default(),
                    })
                    .await
                }
            }
            PlayCommand::PlayLast => {
                if self.playback.is_some() && self.queue.is_some() {
                    self.queue
                        .as_mut()
                        .expect("播放存在时队列应存在")
                        .append(request.item_ids);
                    self.report_progress().await;
                    Ok(())
                } else {
                    self.queue = Some(PlaybackQueue::new(
                        request.item_ids,
                        request.start_index.unwrap_or(0),
                        self.playback_order == PlaybackOrder::Shuffle,
                    )?);
                    self.stop_current(false).await;
                    self.start_current(StartOptions {
                        start_ticks,
                        media_source_id: request.media_source_id,
                        audio_stream_index: request.audio_stream_index,
                        subtitle_stream_index: request.subtitle_stream_index,
                        preferences: PlaybackPreferences::default(),
                    })
                    .await
                }
            }
        }
    }

    async fn start_current(&mut self, options: StartOptions) -> Result<()> {
        let (item_id, playlist_item_id) = self
            .queue
            .as_ref()
            .and_then(PlaybackQueue::current)
            .map(|item| (item.id.clone(), item.playlist_item_id.clone()))
            .ok_or_else(|| anyhow!("当前播放队列为空"))?;
        let start_ticks = options.start_ticks.max(0);
        let preferences = options.preferences;
        let prepared = self
            .prepare_playback(
                item_id,
                playlist_item_id,
                start_ticks,
                options.media_source_id,
                options.audio_stream_index,
                options.subtitle_stream_index,
            )
            .await?;
        let playback_settings = self.skip_settings.snapshot();
        let mpv = match spawn_mpv(MpvLaunch {
            executable: self.mpv_path.clone(),
            extra_args: self.mpv_args.clone(),
            window_title: prepared.item_name.clone(),
            bring_to_front_on_start: playback_settings.bring_to_front_on_start,
            playback_mode: playback_settings.playback_mode,
            url: prepared.url.clone(),
            headers: prepared.headers.clone(),
            start_seconds: prepared.start_ticks as f64 / 10_000_000.0,
            subtitle_url: prepared.subtitle_url.clone(),
            thumbnail_size: prepared
                .thumbnail_profile
                .as_ref()
                .map(TrickplayProfile::display_size),
            quality_selection: quality_selection_for(self.playback_constraints),
            events: self.mpv_events.clone(),
        })
        .await
        {
            Ok(mpv) => mpv,
            Err(error) => return Err(error),
        };

        let thumbnail = if let Some(profile) = prepared.thumbnail_profile.clone() {
            match ThumbnailWorker::start(self.client.clone(), mpv.clone(), profile).await {
                Ok(worker) => Some(worker),
                Err(error) => {
                    warn!(%error, item_id = %prepared.item_id, "初始化 Trickplay 缩略图失败，继续普通播放");
                    None
                }
            }
        } else {
            None
        };

        let playback_mpv = mpv.clone();
        info!(item = %prepared.item_name, item_id = %prepared.item_id, method = prepared.play_method, "正在启动播放");
        self.playback = Some(playback_from_prepared(
            mpv,
            thumbnail,
            prepared,
            preferences,
            self.playback_constraints,
        ));

        if preferences.volume != 100.0 {
            playback_mpv.set_volume(preferences.volume).await?;
        }
        if preferences.muted {
            playback_mpv.set_mute(true).await?;
        }
        if (preferences.playback_rate - 1.0).abs() > f64::EPSILON {
            playback_mpv
                .set_playback_rate(preferences.playback_rate)
                .await?;
        }
        if preferences.paused {
            playback_mpv.set_pause(true).await?;
        }
        Ok(())
    }

    async fn prepare_playback(
        &self,
        item_id: String,
        playlist_item_id: String,
        start_ticks: i64,
        media_source_id: Option<String>,
        audio_stream_index: Option<i32>,
        subtitle_stream_index: Option<i32>,
    ) -> Result<PreparedPlayback> {
        let start_ticks = start_ticks.max(0);
        info!(%item_id, "正在处理 Jellyfin 播放命令");
        let item = self.client.item(&self.user_id, &item_id).await?;
        let info = self
            .client
            .playback_info(
                &self.user_id,
                &item_id,
                start_ticks,
                media_source_id.as_deref(),
                audio_stream_index,
                subtitle_stream_index,
                self.playback_constraints,
            )
            .await?;
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
        let source = media_source_id
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
        let play_method = if source.transcoding_url.is_some() {
            "Transcode"
        } else if source.supports_direct_play {
            "DirectPlay"
        } else {
            "DirectStream"
        };
        let url = self.client.stream_url(
            &item,
            &source_id,
            &play_session_id,
            source.transcoding_url.as_deref(),
        )?;
        let (auth_generation, headers) = self
            .client
            .mpv_http_headers(&source.required_http_headers)
            .await;
        let audio_stream_index = audio_stream_index.or(source.default_audio_stream_index);
        let subtitle_stream_index = match subtitle_stream_index {
            Some(index) => (index >= 0).then_some(index),
            None => source
                .default_subtitle_stream_index
                .filter(|index| *index >= 0),
        };
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
        let thumbnail_profile = select_trickplay(&item, &source_id);

        Ok(PreparedPlayback {
            item_id: item.id,
            playlist_item_id,
            item_name: item.name,
            media_source_id: source_id.clone(),
            play_session_id,
            live_stream_id: source.live_stream_id.clone(),
            play_method,
            audio_stream_index,
            subtitle_stream_index,
            media_streams: source.media_streams.clone(),
            required_http_headers: source.required_http_headers.clone(),
            auth_generation,
            url,
            headers,
            subtitle_url,
            thumbnail_profile,
            start_ticks,
            skip_segments,
        })
    }

    async fn move_queue(&mut self, next: bool) -> Result<()> {
        let target = {
            let Some(queue) = self.queue.as_ref() else {
                return Ok(());
            };
            if next {
                queue.next_index(self.repeat_mode)
            } else {
                queue.previous_index(self.repeat_mode)
            }
        };
        let Some(target) = target else {
            return Ok(());
        };

        let mut preferences = self.playback_preferences();
        preferences.paused = false;
        if let Some(queue) = self.queue.as_mut() {
            queue.set_current(target);
        }
        self.stop_current(false).await;
        self.start_current(StartOptions {
            preferences,
            ..StartOptions::default()
        })
        .await
    }

    async fn set_repeat_mode(&mut self, mode: RepeatMode) -> Result<()> {
        self.repeat_mode = mode;
        if self.playback.is_some() {
            self.report_progress().await;
        }
        info!(?mode, "播放重复模式已更新");
        Ok(())
    }

    async fn set_playback_order(&mut self, order: PlaybackOrder) -> Result<()> {
        self.playback_order = order;
        if let Some(queue) = self.queue.as_mut() {
            queue.set_order(order);
        }
        if self.playback.is_some() {
            self.report_progress().await;
        }
        info!(?order, "播放顺序已更新");
        Ok(())
    }

    async fn set_max_streaming_bitrate(&mut self, bitrate: u64) -> Result<()> {
        let mut constraints = self.playback_constraints;
        constraints.max_streaming_bitrate = Some(bitrate.min(i32::MAX as u64));
        self.set_playback_constraints(constraints).await
    }

    async fn set_quality(&mut self, selection: MpvQualitySelection) -> Result<()> {
        let constraints = match selection {
            MpvQualitySelection::Auto => PlaybackConstraints::default(),
            MpvQualitySelection::Bitrate(bitrate) => PlaybackConstraints::bitrate(bitrate),
            MpvQualitySelection::Resolution { width, height } => {
                PlaybackConstraints::resolution(width, height)
            }
        };
        self.set_playback_constraints(constraints).await?;
        info!(quality = %selection.label(), "已从 MPV 质量菜单选择播放质量");
        Ok(())
    }

    async fn set_playback_constraints(
        &mut self,
        mut constraints: PlaybackConstraints,
    ) -> Result<()> {
        constraints.max_streaming_bitrate = constraints
            .max_streaming_bitrate
            .map(|bitrate| bitrate.clamp(1, i32::MAX as u64));
        if self.playback_constraints == constraints
            && self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.playback_constraints == constraints)
        {
            return Ok(());
        }

        self.playback_constraints = constraints;
        let Some((
            item_id,
            playlist_item_id,
            media_source_id,
            audio_stream_index,
            subtitle_stream_index,
            preferences,
            mpv,
            start_ticks,
        )) = self.playback.as_ref().map(|playback| {
            (
                playback.item_id.clone(),
                playback.playlist_item_id.clone(),
                playback.media_source_id.clone(),
                playback.audio_stream_index,
                playback.subtitle_stream_index,
                PlaybackPreferences {
                    paused: playback.paused,
                    muted: playback.muted,
                    volume: playback.volume,
                    playback_rate: playback.playback_rate,
                },
                playback.mpv.clone(),
                playback.estimated_position_ticks(),
            )
        })
        else {
            info!(?constraints, "已保存下次播放使用的质量限制");
            return Ok(());
        };

        // 先在旧流继续播放时向 Jellyfin 请求新 PlaybackInfo，再在同一个 mpv
        // 进程中替换 URL。这样不会关闭窗口，也不会主动发送暂停命令。
        let mut prepared = self
            .prepare_playback(
                item_id,
                playlist_item_id,
                start_ticks,
                Some(media_source_id),
                audio_stream_index,
                subtitle_stream_index,
            )
            .await?;
        if let Some(playback) = self.playback.as_ref() {
            let resume_ticks = playback.estimated_position_ticks();
            prepared.start_ticks = resume_ticks;
            mark_passed_skip_segments(&mut prepared.skip_segments, resume_ticks);
        }

        mpv.replace_stream(
            prepared.url.clone(),
            prepared.headers.clone(),
            prepared.start_ticks as f64 / 10_000_000.0,
            prepared.subtitle_url.clone(),
            quality_selection_for(constraints),
        )
        .await?;

        let mut previous = self.playback.take().expect("换流前播放状态仍然存在");
        let source_changed = previous.media_source_id != prepared.media_source_id;
        if previous.started {
            let queue = self
                .queue
                .as_ref()
                .map(|queue| queue.items.as_slice())
                .unwrap_or(&[]);
            let report = previous.stop_report(false, queue);
            if let Err(error) = self.client.report_stop(&report).await {
                warn!(%error, "上报质量切换前的 Jellyfin 停止播放失败");
            }
        }

        let thumbnail = if source_changed {
            if let Some(thumbnail) = previous.thumbnail.take() {
                thumbnail.stop().await;
            }
            match prepared.thumbnail_profile.clone() {
                Some(profile) => {
                    match ThumbnailWorker::start(self.client.clone(), mpv.clone(), profile).await {
                        Ok(worker) => Some(worker),
                        Err(error) => {
                            warn!(%error, item_id = %prepared.item_id, "更新 Trickplay 缩略图失败，继续普通播放");
                            None
                        }
                    }
                }
                None => None,
            }
        } else {
            previous.thumbnail.take()
        };

        info!(
            item = %prepared.item_name,
            method = prepared.play_method,
            ?constraints,
            "已在现有 mpv 进程中切换播放质量"
        );
        self.playback = Some(playback_from_prepared(
            mpv,
            thumbnail,
            prepared,
            preferences,
            constraints,
        ));
        Ok(())
    }

    fn playback_preferences(&self) -> PlaybackPreferences {
        self.playback
            .as_ref()
            .map(|playback| PlaybackPreferences {
                paused: playback.paused,
                muted: playback.muted,
                volume: playback.volume,
                playback_rate: playback.playback_rate,
            })
            .unwrap_or_default()
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
            | MpvEvent::Muted { id, .. }
            | MpvEvent::Volume { id, .. }
            | MpvEvent::AudioTracks { id, .. }
            | MpvEvent::Preview { id, .. }
            | MpvEvent::Quality { id, .. }
            | MpvEvent::Ended { id, .. }
            | MpvEvent::Terminated { id, .. } => *id,
        };
        if self.playback.as_ref().map(|state| state.mpv.id) != Some(event_id) {
            return Ok(());
        }

        match event {
            MpvEvent::FileLoaded { .. } => {
                let should_report = self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| !playback.started);
                if should_report {
                    let report = {
                        let playback = self.playback.as_mut().expect("已核对播放 ID");
                        playback.started = true;
                        let queue = self
                            .queue
                            .as_ref()
                            .ok_or_else(|| anyhow!("播放已加载但当前没有播放队列"))?;
                        playback.report(queue, self.repeat_mode, self.playback_order)
                    };
                    self.client.report_start(&report).await?;
                    let item_name = self
                        .playback
                        .as_ref()
                        .map(|playback| playback.item_name.as_str())
                        .unwrap_or("未知媒体");
                    info!(item = item_name, "播放已开始并上报 Jellyfin");
                }
                self.refresh_mpv_headers().await;
            }
            MpvEvent::Position { seconds, .. } => {
                self.handle_position(seconds).await?;
            }
            MpvEvent::Paused { paused, .. } => {
                self.playback.as_mut().expect("已核对播放 ID").paused = paused;
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
            MpvEvent::Quality { selection, .. } => {
                self.set_quality(selection).await?;
            }
            MpvEvent::Ended { reason, .. } => match reason {
                MpvEndReason::Eof => self.handle_natural_playback_end().await?,
                MpvEndReason::Stopped => self.stop_after_mpv_end(false).await,
                MpvEndReason::Failed => self.stop_after_mpv_end(true).await,
            },
            MpvEvent::Terminated { failed, .. } => {
                // 没有收到 end-file 的进程退出（例如用户直接关闭窗口）也必须
                // 结束整个队列，而不是把它误当作当前条目自然播完。
                self.stop_after_mpv_end(failed).await;
            }
        }
        Ok(())
    }

    async fn stop_after_mpv_end(&mut self, failed: bool) {
        self.queue = None;
        self.stop_current(failed).await;
    }

    async fn handle_natural_playback_end(&mut self) -> Result<()> {
        let next = self
            .queue
            .as_ref()
            .and_then(|queue| queue.next_index(self.repeat_mode));
        let Some(next) = next else {
            self.queue = None;
            self.stop_current(false).await;
            return Ok(());
        };

        if let Some(queue) = self.queue.as_mut() {
            queue.set_current(next);
        }
        let mut preferences = self.playback_preferences();
        preferences.paused = false;
        self.stop_current(false).await;
        self.start_current(StartOptions {
            preferences,
            ..StartOptions::default()
        })
        .await
    }

    async fn handle_position(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() {
            return Ok(());
        }

        let position_ticks = (seconds.max(0.0) * 10_000_000.0) as i64;
        let settings = self.skip_settings.snapshot();
        let skip = {
            let playback = self.playback.as_mut().expect("已核对播放 ID");
            if !playback.started {
                return Ok(());
            }
            playback.position_ticks = position_ticks;
            playback.last_position_update = Instant::now();
            next_skip_target(&mut playback.skip_segments, position_ticks, &settings)
        };

        let Some((kind, target_ticks)) = skip else {
            return Ok(());
        };
        let playback = self.playback.as_mut().expect("播放状态仍然存在");
        playback
            .mpv
            .seek(target_ticks as f64 / 10_000_000.0)
            .await?;
        playback.position_ticks = target_ticks;
        playback.last_position_update = Instant::now();
        info!(
            segment_type = %kind,
            target_seconds = target_ticks as f64 / 10_000_000.0,
            "已自动跳过 Jellyfin 媒体分段"
        );
        self.report_progress().await;
        Ok(())
    }

    async fn report_progress(&mut self) {
        let Some(playback) = &self.playback else {
            return;
        };
        let Some(queue) = &self.queue else {
            return;
        };
        if !playback.started {
            return;
        }
        let report = playback.report(queue, self.repeat_mode, self.playback_order);
        if let Err(error) = self.client.report_progress(&report).await {
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
            let queue = self
                .queue
                .as_ref()
                .map(|queue| queue.items.as_slice())
                .unwrap_or(&[]);
            let report = playback.stop_report(failed, queue);
            if let Err(error) = self.client.report_stop(&report).await {
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

fn quality_selection_for(constraints: PlaybackConstraints) -> MpvQualitySelection {
    match (constraints.max_width, constraints.max_height) {
        (Some(width), Some(height)) => MpvQualitySelection::Resolution { width, height },
        _ => constraints
            .max_streaming_bitrate
            .map(MpvQualitySelection::Bitrate)
            .unwrap_or(MpvQualitySelection::Auto),
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
    fn derives_mpv_quality_menu_state_from_playback_constraints() {
        assert_eq!(
            quality_selection_for(PlaybackConstraints::default()),
            MpvQualitySelection::Auto
        );
        assert_eq!(
            quality_selection_for(PlaybackConstraints::bitrate(8_000_000)),
            MpvQualitySelection::Bitrate(8_000_000)
        );
        assert_eq!(
            quality_selection_for(PlaybackConstraints::resolution(1_280, 720)),
            MpvQualitySelection::Resolution {
                width: 1_280,
                height: 720,
            }
        );
    }

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
    fn queue_advances_and_repeats_in_order() {
        let mut queue = PlaybackQueue::new(
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            1,
            false,
        )
        .unwrap();

        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("b"));
        assert_eq!(queue.next_index(RepeatMode::RepeatNone), Some(2));
        assert_eq!(queue.previous_index(RepeatMode::RepeatNone), Some(0));
        queue.set_current(2);
        assert_eq!(queue.next_index(RepeatMode::RepeatNone), None);
        assert_eq!(queue.next_index(RepeatMode::RepeatOne), Some(2));
        assert_eq!(queue.next_index(RepeatMode::RepeatAll), Some(0));
    }

    #[test]
    fn queue_inserts_next_and_restores_sorted_order() {
        let mut queue = PlaybackQueue::new(
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            1,
            false,
        )
        .unwrap();
        queue.insert_next(vec!["x".to_owned(), "y".to_owned()]);
        assert_eq!(
            queue
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "x", "y", "c"]
        );

        queue.set_order(PlaybackOrder::Shuffle);
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("b"));
        queue.set_order(PlaybackOrder::Default);
        assert_eq!(
            queue
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "x", "y"]
        );
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("b"));
    }

    fn media_segment(kind: &str, start_ticks: i64, end_ticks: i64) -> MediaSegment {
        MediaSegment {
            segment_type: Some(kind.to_owned()),
            start_ticks: Some(start_ticks),
            end_ticks: Some(end_ticks),
        }
    }
}
