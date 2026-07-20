use std::{ffi::OsString, io::IsTerminal, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use url::Url;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "jellyfin-mpv",
    version,
    about = "无界面的 Jellyfin 外部 mpv 投屏接收端"
)]
pub struct Config {
    /// Jellyfin 根地址，例如 <http://192.168.1.2:8096>
    #[arg(long, env = "JELLYFIN_SERVER")]
    pub server: Url,

    /// Jellyfin 用户 Access Token；与用户名登录二选一
    #[arg(
        long,
        env = "JELLYFIN_TOKEN",
        hide_env_values = true,
        conflicts_with = "username"
    )]
    pub token: Option<String>,

    /// Jellyfin 用户名；未提供密码时在终端安全提示输入
    #[arg(long, env = "JELLYFIN_USERNAME", conflicts_with = "token")]
    pub username: Option<String>,

    /// Jellyfin 密码；建议使用安全提示或 `JELLYFIN_PASSWORD` 环境变量
    #[arg(
        long,
        env = "JELLYFIN_PASSWORD",
        hide_env_values = true,
        requires = "username"
    )]
    pub password: Option<String>,

    /// Jellyfin 中显示的设备名称
    #[arg(long, env = "JELLYFIN_DEVICE_NAME")]
    pub device_name: Option<String>,

    /// 稳定的设备 ID；同一设备重启后应保持不变
    #[arg(long, env = "JELLYFIN_DEVICE_ID")]
    pub device_id: Option<String>,

    /// mpv 可执行文件路径
    #[arg(long, env = "MPV_PATH", default_value = "mpv")]
    pub mpv_path: PathBuf,

    /// 追加传给 mpv 的参数，可重复使用
    #[arg(long = "mpv-arg", env = "MPV_ARGS", value_delimiter = ' ')]
    pub mpv_args: Vec<OsString>,
}

impl Config {
    pub fn normalize(mut self) -> Result<Self> {
        self.token = self
            .token
            .take()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        self.username = self
            .username
            .take()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());

        if self.token.is_none() && self.username.is_none() {
            bail!("请提供 --username（用户名登录）或 --token（用户 Access Token）");
        }
        if self.username.is_some() && self.password.is_none() {
            if !std::io::stdin().is_terminal() {
                bail!("后台运行时请通过 JELLYFIN_PASSWORD 提供密码");
            }
            self.password = Some(
                rpassword::prompt_password("Jellyfin 密码: ").context("读取 Jellyfin 密码失败")?,
            );
        }
        if self.password.as_deref().is_some_and(str::is_empty) {
            bail!("Jellyfin 密码不能为空");
        }

        if !matches!(self.server.scheme(), "http" | "https") {
            bail!("JELLYFIN_SERVER 必须使用 http 或 https");
        }

        self.server.set_query(None);
        self.server.set_fragment(None);
        let normalized_path = format!("{}/", self.server.path().trim_end_matches('/'));
        self.server.set_path(&normalized_path);

        let host = hostname::get()
            .context("无法读取主机名")?
            .to_string_lossy()
            .into_owned();
        if self.device_name.as_deref().is_none_or(str::is_empty) {
            self.device_name = Some(format!("{host} mpv"));
        }
        if self.device_id.as_deref().is_none_or(str::is_empty) {
            self.device_id = Some(format!("jellyfin-mpv-{host}"));
        }

        Ok(self)
    }

    pub fn device_name(&self) -> &str {
        self.device_name.as_deref().expect("配置已 normalize")
    }

    pub fn device_id(&self) -> &str {
        self.device_id.as_deref().expect("配置已 normalize")
    }

    pub fn token(&self) -> &str {
        self.token.as_deref().expect("认证完成后必须有 token")
    }
}
