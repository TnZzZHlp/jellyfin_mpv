use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow, bail};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    jellyfin::JellyfinClient,
    mpv::{MpvEvent, MpvHandle, spawn_mpv},
    protocol::{PlayRequest, PlaybackReport, PlaybackStopReport, RemoteCommand},
};

pub struct Player {
    client: Arc<JellyfinClient>,
    user_id: String,
    mpv_path: PathBuf,
    mpv_args: Vec<OsString>,
    mpv_events: mpsc::Sender<MpvEvent>,
    playback: Option<Playback>,
}

struct Playback {
    mpv: MpvHandle,
    item_id: String,
    item_name: String,
    media_source_id: String,
    play_session_id: String,
    live_stream_id: Option<String>,
    play_method: &'static str,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
    position_ticks: i64,
    paused: bool,
    muted: bool,
    volume: f64,
    started: bool,
}

impl Playback {
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
    ) -> Self {
        Self {
            client,
            user_id,
            mpv_path,
            mpv_args,
            mpv_events,
            playback: None,
        }
    }

    pub async fn run(
        &mut self,
        mut commands: mpsc::Receiver<RemoteCommand>,
        mut mpv_events: mpsc::Receiver<MpvEvent>,
    ) -> Result<()> {
        let mut progress = interval(Duration::from_secs(5));
        progress.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
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
            RemoteCommand::Seek(ticks) => {
                let playback = self.active_mut()?;
                let ticks = ticks.max(0);
                playback.mpv.seek(ticks as f64 / 10_000_000.0).await?;
                playback.position_ticks = ticks;
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
        }
    }

    async fn play(&mut self, request: PlayRequest) -> Result<()> {
        let index = request.start_index.unwrap_or(0);
        let item_id = request
            .item_ids
            .get(index)
            .or_else(|| request.item_ids.first())
            .ok_or_else(|| anyhow!("播放命令没有 ItemIds"))?
            .clone();
        let start_ticks = request.start_position_ticks.unwrap_or(0).max(0);

        self.stop_current(false).await;
        let item = self.client.item(&self.user_id, &item_id).await?;
        let info = self
            .client
            .playback_info(
                &self.user_id,
                &item_id,
                start_ticks,
                request.media_source_id.as_deref(),
                request.audio_stream_index,
                request.subtitle_stream_index,
            )
            .await?;
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
        let headers = self.client.mpv_http_headers(&source.required_http_headers);
        let audio_stream_index = request
            .audio_stream_index
            .or(source.default_audio_stream_index);
        let subtitle_stream_index = request
            .subtitle_stream_index
            .or(source.default_subtitle_stream_index);
        let mpv = spawn_mpv(
            self.mpv_path.clone(),
            self.mpv_args.clone(),
            url,
            headers,
            start_ticks as f64 / 10_000_000.0,
            self.mpv_events.clone(),
        )
        .await?;

        info!(item = %item.name, item_id = %item.id, method = play_method, "正在启动播放");
        self.playback = Some(Playback {
            mpv,
            item_id: item.id,
            item_name: item.name,
            media_source_id: source_id,
            play_session_id,
            live_stream_id: source.live_stream_id.clone(),
            play_method,
            audio_stream_index,
            subtitle_stream_index,
            position_ticks: start_ticks,
            paused: false,
            muted: false,
            volume: 100.0,
            started: false,
        });
        Ok(())
    }

    async fn handle_mpv_event(&mut self, event: MpvEvent) -> Result<()> {
        let event_id = match &event {
            MpvEvent::FileLoaded { id }
            | MpvEvent::Position { id, .. }
            | MpvEvent::Paused { id, .. }
            | MpvEvent::Muted { id, .. }
            | MpvEvent::Volume { id, .. }
            | MpvEvent::Ended { id, .. }
            | MpvEvent::Terminated { id, .. } => *id,
        };
        if self.playback.as_ref().map(|state| state.mpv.id) != Some(event_id) {
            return Ok(());
        }

        match event {
            MpvEvent::FileLoaded { .. } => {
                let playback = self.playback.as_mut().expect("已核对播放 ID");
                if !playback.started {
                    playback.started = true;
                    self.client.report_start(&playback.report()).await?;
                    info!(item = %playback.item_name, "播放已开始并上报 Jellyfin");
                }
            }
            MpvEvent::Position { seconds, .. } => {
                let playback = self.playback.as_mut().expect("已核对播放 ID");
                if playback.started && seconds.is_finite() {
                    playback.position_ticks = (seconds.max(0.0) * 10_000_000.0) as i64;
                }
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
            MpvEvent::Ended { failed, .. } | MpvEvent::Terminated { failed, .. } => {
                self.stop_current(failed).await;
            }
        }
        Ok(())
    }

    async fn report_progress(&self) {
        let Some(playback) = &self.playback else {
            return;
        };
        if !playback.started {
            return;
        }
        if let Err(error) = self.client.report_progress(&playback.report()).await {
            warn!(%error, "上报 Jellyfin 播放进度失败");
        }
    }

    async fn stop_current(&mut self, failed: bool) {
        let Some(playback) = self.playback.take() else {
            return;
        };
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
