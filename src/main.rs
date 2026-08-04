#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod config;
mod credential_store;
mod jellyfin;
mod login_ui;
mod mpv;
mod player;
mod protocol;
mod settings_ui;
mod single_instance;
mod skip_settings;
mod thumbnail;
mod tray;

use std::{
    env,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt::time::ChronoLocal};

use crate::{
    config::ConfigArgs, credential_store::save_login, jellyfin::JellyfinClient,
    login_ui::show_error_message, mpv::MpvEvent, player::Player, settings_ui::SettingsContext,
    skip_settings::SkipSettingsStore, tray::TrayIcon,
};

#[tokio::main]
async fn main() {
    let _single_instance = match single_instance::acquire() {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            show_error_message("Jellyfin MPV Cast 已在运行，请使用通知区域中的现有实例。");
            return;
        }
        Err(error) => {
            show_error_message(&format!("无法检查程序是否已在运行：{error:#}"));
            return;
        }
    };

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
    let initial_connection = loop {
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
    let skip_settings = Arc::new(match SkipSettingsStore::load() {
        Ok(settings) => settings,
        Err(error) => {
            warn!(%error, "读取本地设置失败，使用默认设置");
            SkipSettingsStore::default_store()
        }
    });
    let current_login = Arc::new(RwLock::new(config.current_login()));
    let (connection_updates, mut connection_update_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let tray = match TrayIcon::start(
        shutdown.clone(),
        format!("Jellyfin MPV Cast - {device_name}"),
        log_path,
        SettingsContext {
            login: Arc::clone(&current_login),
            skip_settings: Arc::clone(&skip_settings),
            connection_updates,
        },
    ) {
        Ok(tray) => Some(tray),
        Err(error) => {
            warn!(%error, "创建 Windows 托盘图标失败，程序继续在后台运行");
            None
        }
    };
    let mut connection = Some(initial_connection);
    let run_result = 'connections: loop {
        let (client, user) = if let Some(connected) = connection.take() {
            connected
        } else {
            loop {
                match JellyfinClient::connect(config.clone()).await {
                    Ok(connected) => break connected,
                    Err(error) => {
                        warn!(%error, server = %config.server, "重新连接 Jellyfin 失败");
                        show_error_message(&format!(
                            "无法连接新的 Jellyfin 服务器：{error:#}\n\n请通过托盘菜单重新打开设置进行修改。"
                        ));
                        tokio::select! {
                            () = shutdown.cancelled() => break 'connections Ok(()),
                            update = connection_update_rx.recv() => {
                                let Some(login) = update else {
                                    break 'connections Ok(());
                                };
                                config.apply_login(login)?;
                            }
                        }
                    }
                }
            }
        };

        let client = Arc::new(client);
        if let Err(error) = client.register_capabilities().await {
            warn!(error = %format!("{error:#}"), "注册 Jellyfin 播放设备失败，将在 WebSocket 连接后重试");
        }
        info!(
            server = %config.server,
            user = %user.name,
            device = %device_name,
            "已连接 Jellyfin，等待远程播放命令"
        );

        let session_shutdown = shutdown.child_token();
        let (command_tx, command_rx) = mpsc::channel(64);
        let (mpv_event_tx, mpv_event_rx) = mpsc::channel::<MpvEvent>(128);
        let ws_client = Arc::clone(&client);
        let ws_shutdown = session_shutdown.clone();
        let ws_task = tokio::spawn(async move {
            ws_client
                .websocket_supervisor(command_tx, ws_shutdown)
                .await;
        });

        let mut player = Player::new(
            Arc::clone(&client),
            user.id,
            mpv_path.clone(),
            mpv_args.clone(),
            mpv_event_tx,
            Arc::clone(&skip_settings),
        );
        let mut player_run =
            Box::pin(player.run(command_rx, mpv_event_rx, session_shutdown.clone()));
        let (next_login, session_result) = tokio::select! {
            result = &mut player_run => (None, result),
            Some(login) = connection_update_rx.recv() => {
                session_shutdown.cancel();
                (Some(login), player_run.await)
            }
        };

        session_shutdown.cancel();
        if let Err(error) = ws_task.await {
            error!(%error, "WebSocket 任务异常退出");
        }
        session_result?;

        if let Some(login) = next_login {
            config.apply_login(login)?;
            info!(server = %config.server, "服务器设置已更新，正在重新连接");
            continue;
        }
        break Ok(());
    };

    shutdown.cancel();
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
        .with_timer(ChronoLocal::rfc_3339())
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
