#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod config;
mod credential_store;
mod jellyfin;
mod login_ui;
mod mpv;
mod player;
mod protocol;
mod tray;

use std::{
    env,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    config::ConfigArgs, credential_store::save_login, jellyfin::JellyfinClient,
    login_ui::show_error_message, mpv::MpvEvent, player::Player, tray::TrayIcon,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        error!(error = ?error, "程序异常退出");
        show_error_message(&format!("{error:#}"));
    }
}

async fn run() -> Result<()> {
    let log_path = init_logging()?;
    info!(path = %log_path.display(), "日志系统已启动");

    let Some(mut config) = ConfigArgs::parse().resolve()? else {
        return Ok(());
    };
    let (client, user) = loop {
        match JellyfinClient::connect(config.clone()).await {
            Ok(connected) => break connected,
            Err(error) if config.loaded_login() => {
                warn!(%error, "保存的登录信息无效，重新显示登录窗口");
                let Some(updated) = config.prompt_for_login()? else {
                    return Ok(());
                };
                config = updated;
            }
            Err(error) => return Err(error),
        }
    };

    let server = config.server.clone();
    let device_name = config.device_name().to_owned();
    let mpv_path = config.mpv_path.clone();
    let mpv_args = config.mpv_args.clone();
    let login_to_save = config.login_to_save();
    if let Some(login) = login_to_save {
        match save_login(&login) {
            Ok(()) => info!("登录信息已保存到 Windows 凭据管理器"),
            Err(error) => warn!(%error, "保存登录信息失败，程序继续运行"),
        }
    }
    let client = Arc::new(client);
    client.register_capabilities().await?;

    info!(
        server = %server,
        user = %user.name,
        device = %device_name,
        "已连接 Jellyfin，等待远程播放命令"
    );

    let shutdown = CancellationToken::new();
    let tray = match TrayIcon::start(
        shutdown.clone(),
        format!("Jellyfin MPV Cast - {device_name}"),
        log_path,
    ) {
        Ok(tray) => Some(tray),
        Err(error) => {
            warn!(%error, "创建 Windows 托盘图标失败，程序继续在后台运行");
            None
        }
    };
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

    let run_result = player.run(command_rx, mpv_event_rx, shutdown.clone()).await;
    shutdown.cancel();
    if let Err(error) = ws_task.await {
        error!(%error, "WebSocket 任务异常退出");
    }
    if let Some(tray) = tray {
        tray.close();
    }

    run_result
}

fn init_logging() -> Result<PathBuf> {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let log_directory = base.join("jellyfin_mpv");
    fs::create_dir_all(&log_directory).context("创建日志目录失败")?;
    let log_path = log_directory.join("jellyfin_mpv.log");
    let log_file = Arc::new(open_log_file(&log_path)?);
    let make_writer = move || log_file.try_clone().expect("克隆日志文件句柄失败");
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("jellyfin_mpv=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(make_writer)
        .compact()
        .init();
    Ok(log_path)
}

fn open_log_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("无法打开日志文件 {}", path.display()))
}
