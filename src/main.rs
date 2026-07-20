mod config;
mod jellyfin;
mod mpv;
mod player;
mod protocol;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, jellyfin::JellyfinClient, mpv::MpvEvent, player::Player};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Config::parse().normalize()?;
    let server = config.server.clone();
    let device_name = config.device_name().to_owned();
    let mpv_path = config.mpv_path.clone();
    let mpv_args = config.mpv_args.clone();
    let (client, user) = JellyfinClient::connect(config).await?;
    let client = Arc::new(client);
    client.register_capabilities().await?;

    info!(
        server = %server,
        user = %user.name,
        device = %device_name,
        "已连接 Jellyfin，等待远程播放命令"
    );

    let shutdown = CancellationToken::new();
    let (command_tx, command_rx) = mpsc::channel(64);
    let (mpv_event_tx, mpv_event_rx) = mpsc::channel::<MpvEvent>(128);

    let ws_client = Arc::clone(&client);
    let ws_shutdown = shutdown.clone();
    let ws_task = tokio::spawn(async move {
        ws_client
            .websocket_supervisor(command_tx, ws_shutdown)
            .await;
    });

    let mut player = Player::new(
        Arc::clone(&client),
        user.id,
        mpv_path,
        mpv_args,
        mpv_event_tx,
    );

    let run_result = player.run(command_rx, mpv_event_rx).await;
    shutdown.cancel();
    if let Err(error) = ws_task.await {
        error!(%error, "WebSocket 任务异常退出");
    }

    run_result
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("jellyfin_mpv=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
