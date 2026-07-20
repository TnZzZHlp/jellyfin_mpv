use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::Parser;
use url::Url;

use crate::{
    credential_store::{SavedLogin, load_login},
    login_ui::{LoginDefaults, prompt_login},
};

#[derive(Clone, Parser)]
#[command(name = "jellyfin-mpv", version, about = "Jellyfin 外部 mpv 投屏接收端")]
pub struct ConfigArgs {
    /// Jellyfin 根地址，例如 <http://192.168.1.2:8096>
    #[arg(long, env = "JELLYFIN_SERVER")]
    pub server: Option<Url>,

    /// Jellyfin 用户名
    #[arg(long, env = "JELLYFIN_USERNAME")]
    pub username: Option<String>,

    /// Base64 编码的 Jellyfin 密码；登录窗口中则直接输入明文密码
    #[arg(long, env = "JELLYFIN_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// 缺少登录参数时不显示登录窗口，直接报错
    #[arg(long, env = "JELLYFIN_NO_UI", default_value_t = false)]
    pub no_ui: bool,

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

#[derive(Clone)]
pub struct Config {
    pub server: Url,
    pub username: Option<String>,
    pub password: Option<String>,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub mpv_path: PathBuf,
    pub mpv_args: Vec<OsString>,
    persist_login: bool,
    loaded_login: bool,
}

impl ConfigArgs {
    pub fn resolve(self) -> Result<Option<Config>> {
        let mut server = self.server;
        let mut username = normalize_username(self.username);
        let mut password = self.password.map(decode_password).transpose()?;
        let mut persist_login = false;
        let mut loaded_login = false;
        let has_explicit_login = server.is_some() || username.is_some() || password.is_some();

        if server.is_none() || username.is_none() || password.is_none() {
            if self.no_ui {
                bail!(missing_login_message(
                    server.is_none(),
                    username.is_none(),
                    password.is_none()
                ));
            }

            let saved = match load_login() {
                Ok(saved) => saved,
                Err(error) => {
                    tracing::warn!(%error, "读取 Windows 凭据管理器失败，使用空白登录窗口");
                    None
                }
            };

            if !has_explicit_login
                && let Some(saved) = saved.as_ref()
                && let Ok(saved_server) = Url::parse(&saved.server)
                && matches!(saved_server.scheme(), "http" | "https")
                && !saved.username.trim().is_empty()
                && validate_password(&saved.password).is_ok()
            {
                server = Some(saved_server);
                username = normalize_username(Some(saved.username.clone()));
                password = Some(saved.password.clone());
                loaded_login = true;
            }

            if let (Some(saved_server), Some(saved_username), Some(saved_password)) =
                (server.as_ref(), username.as_ref(), password.as_ref())
            {
                return Config {
                    server: saved_server.clone(),
                    username: Some(saved_username.clone()),
                    password: Some(saved_password.clone()),
                    device_name: self.device_name,
                    device_id: self.device_id,
                    mpv_path: self.mpv_path,
                    mpv_args: self.mpv_args,
                    persist_login,
                    loaded_login,
                }
                .normalize()
                .map(Some);
            }

            let defaults = LoginDefaults {
                server: server
                    .as_ref()
                    .map(Url::to_string)
                    .or_else(|| saved.as_ref().map(|login| login.server.clone()))
                    .unwrap_or_else(|| "http://127.0.0.1:8096".to_owned()),
                username: username
                    .clone()
                    .or_else(|| saved.as_ref().map(|login| login.username.clone()))
                    .unwrap_or_default(),
                password: password
                    .clone()
                    .or_else(|| saved.map(|login| login.password))
                    .unwrap_or_default(),
            };
            let Some(input) = prompt_login(defaults)? else {
                return Ok(None);
            };
            server = Some(input.server);
            username = Some(input.username);
            password = Some(input.password);
            persist_login = true;
        }

        Config {
            server: server.expect("登录配置已补齐"),
            username,
            password,
            device_name: self.device_name,
            device_id: self.device_id,
            mpv_path: self.mpv_path,
            mpv_args: self.mpv_args,
            persist_login,
            loaded_login,
        }
        .normalize()
        .map(Some)
    }
}

impl Config {
    fn normalize(mut self) -> Result<Self> {
        self.username = normalize_username(self.username);
        if self.username.is_none() {
            bail!("Jellyfin 用户名不能为空");
        }

        let mut password = self.password.take().context("Jellyfin 密码不能为空")?;
        let trimmed_len = password.trim_end_matches(['\r', '\n']).len();
        password.truncate(trimmed_len);
        validate_password(&password)?;
        self.password = Some(password);

        if !matches!(self.server.scheme(), "http" | "https") {
            bail!("Jellyfin 地址必须使用 http 或 https");
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

    pub fn login_to_save(&self) -> Option<SavedLogin> {
        self.persist_login.then(|| self.current_login())
    }

    pub fn current_login(&self) -> SavedLogin {
        SavedLogin {
            server: self.server.to_string(),
            username: self
                .username
                .as_deref()
                .expect("配置已 normalize")
                .to_owned(),
            password: self
                .password
                .as_deref()
                .expect("配置已 normalize")
                .to_owned(),
        }
    }

    pub fn apply_login(&mut self, login: SavedLogin) -> Result<()> {
        let mut updated = self.clone();
        updated.server = Url::parse(&login.server).context("设置中的 Jellyfin 地址格式不正确")?;
        updated.username = Some(login.username);
        updated.password = Some(login.password);
        updated.persist_login = false;
        updated.loaded_login = false;
        *self = updated.normalize()?;
        Ok(())
    }

    pub fn loaded_login(&self) -> bool {
        self.loaded_login
    }

    pub fn prompt_for_login(mut self) -> Result<Option<Self>> {
        let defaults = LoginDefaults {
            server: self.server.to_string(),
            username: self.username.clone().unwrap_or_default(),
            password: self.password.clone().unwrap_or_default(),
        };
        let Some(input) = prompt_login(defaults)? else {
            return Ok(None);
        };
        self.server = input.server;
        self.username = Some(input.username);
        self.password = Some(input.password);
        self.persist_login = true;
        self.loaded_login = false;
        self.normalize().map(Some)
    }
}

fn normalize_username(username: Option<String>) -> Option<String> {
    username
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn decode_password(encoded_password: String) -> Result<String> {
    if encoded_password.trim().is_empty() {
        bail!("请通过 --password 提供 Base64 编码的 Jellyfin 密码");
    }
    let password_bytes = STANDARD
        .decode(encoded_password.trim())
        .context("--password / JELLYFIN_PASSWORD 不是有效的 Base64")?;
    let password = String::from_utf8(password_bytes)
        .context("--password / JELLYFIN_PASSWORD 解码后不是有效的 UTF-8 密码")?;
    Ok(password)
}

fn validate_password(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("Jellyfin 密码不能为空");
    }
    if password.contains('\0') {
        bail!("密码包含 NUL 字符；Base64 原文可能使用了 UTF-16，请先用 UTF-8 编码密码");
    }
    if password.starts_with('\u{feff}') {
        bail!("密码包含 UTF-8 BOM；生成 Base64 时请使用不带 BOM 的 UTF-8");
    }
    Ok(())
}

fn missing_login_message(server: bool, username: bool, password: bool) -> String {
    let mut missing = Vec::new();
    if server {
        missing.push("--server");
    }
    if username {
        missing.push("--username");
    }
    if password {
        missing.push("--password");
    }
    format!("缺少登录参数：{}", missing.join("、"))
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use clap::Parser;

    use super::ConfigArgs;
    use crate::credential_store::SavedLogin;

    fn parse(args: &[&str]) -> ConfigArgs {
        ConfigArgs::try_parse_from(args).unwrap()
    }

    #[test]
    fn decodes_base64_password_without_opening_ui() {
        let encoded = STANDARD.encode("pāss word");
        let config = parse(&[
            "jellyfin-mpv",
            "--server",
            "http://localhost:8096",
            "--username",
            "alice",
            "--password",
            &encoded,
        ])
        .resolve()
        .unwrap()
        .unwrap();
        assert_eq!(config.password.as_deref(), Some("pāss word"));
    }

    #[test]
    fn rejects_invalid_base64_password() {
        let config = parse(&[
            "jellyfin-mpv",
            "--server",
            "http://localhost:8096",
            "--username",
            "alice",
            "--password",
            "not base64!",
        ]);
        assert!(config.resolve().is_err());
    }

    #[test]
    fn rejects_utf16_and_bom_passwords() {
        for password in ["p\0a\0s\0s\0", "\u{feff}password"] {
            let encoded = STANDARD.encode(password);
            let config = parse(&[
                "jellyfin-mpv",
                "--server",
                "http://localhost:8096",
                "--username",
                "alice",
                "--password",
                &encoded,
            ]);
            assert!(config.resolve().is_err());
        }
    }

    #[test]
    fn removes_trailing_newlines_from_password() {
        let encoded = STANDARD.encode("pass word\r\n\n");
        let config = parse(&[
            "jellyfin-mpv",
            "--server",
            "http://localhost:8096",
            "--username",
            "alice",
            "--password",
            &encoded,
        ])
        .resolve()
        .unwrap()
        .unwrap();
        assert_eq!(config.password.as_deref(), Some("pass word"));
    }

    #[test]
    fn no_ui_reports_all_missing_login_fields() {
        let error = parse(&["jellyfin-mpv", "--no-ui"])
            .resolve()
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("--server"));
        assert!(error.contains("--username"));
        assert!(error.contains("--password"));
    }

    #[test]
    fn applies_server_login_from_the_settings_page() {
        let encoded = STANDARD.encode("old-password");
        let mut config = parse(&[
            "jellyfin-mpv",
            "--server",
            "http://old.example:8096",
            "--username",
            "old-user",
            "--password",
            &encoded,
        ])
        .resolve()
        .unwrap()
        .unwrap();

        config
            .apply_login(SavedLogin {
                server: "https://new.example/jellyfin".to_owned(),
                username: " new-user ".to_owned(),
                password: "new-password".to_owned(),
            })
            .unwrap();

        assert_eq!(config.server.as_str(), "https://new.example/jellyfin/");
        assert_eq!(config.username.as_deref(), Some("new-user"));
        assert_eq!(config.password.as_deref(), Some("new-password"));
    }
}
