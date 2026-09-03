use anyhow::Result;
use serde::{Deserialize, Serialize};

#[cfg(windows)]
const CREDENTIAL_TARGET: &str = "jellyfin_mpv/login";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedLogin {
    pub server: String,
    pub username: String,
    pub password: String,
}

#[cfg_attr(not(windows), allow(clippy::missing_const_for_fn))]
pub fn load_login() -> Result<Option<SavedLogin>> {
    platform::load()
}

pub fn save_login(login: &SavedLogin) -> Result<()> {
    platform::save(login)
}

#[cfg(windows)]
mod platform {
    use std::{ptr::null_mut, slice};

    use anyhow::{Context, Result, bail};
    use windows_sys::Win32::{
        Foundation::{ERROR_NOT_FOUND, FILETIME, GetLastError},
        Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
            CredWriteW,
        },
    };

    use super::{CREDENTIAL_TARGET, SavedLogin};

    pub fn load() -> Result<Option<SavedLogin>> {
        unsafe {
            let target = wide(CREDENTIAL_TARGET);
            let mut credential = null_mut();
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut credential) == 0 {
                let error = GetLastError();
                if error == ERROR_NOT_FOUND {
                    return Ok(None);
                }
                bail!("CredReadW 失败，Windows 错误码 {error}");
            }
            if credential.is_null() {
                bail!("Windows 凭据管理器返回了空凭据");
            }

            let stored = &*credential;
            let bytes = if stored.CredentialBlobSize == 0 {
                Vec::new()
            } else if stored.CredentialBlob.is_null() {
                CredFree(credential.cast());
                bail!("Windows 凭据管理器中的密码数据为空");
            } else {
                slice::from_raw_parts(stored.CredentialBlob, stored.CredentialBlobSize as usize)
                    .to_vec()
            };
            CredFree(credential.cast());

            let login =
                serde_json::from_slice(&bytes).context("Windows 凭据管理器中的登录信息已损坏")?;
            Ok(Some(login))
        }
    }

    pub fn save(login: &SavedLogin) -> Result<()> {
        let mut blob = serde_json::to_vec(login).context("序列化登录信息失败")?;
        let blob_size = u32::try_from(blob.len()).context("登录信息过长，无法保存")?;
        let mut target = wide(CREDENTIAL_TARGET);
        let mut username = wide(&login.username);
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: null_mut(),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: blob_size,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: username.as_mut_ptr(),
        };

        unsafe {
            if CredWriteW(&raw const credential, 0) == 0 {
                let error = GetLastError();
                bail!("CredWriteW 失败，Windows 错误码 {error}");
            }
        }
        Ok(())
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Result, bail};

    use super::SavedLogin;

    #[allow(clippy::unnecessary_wraps)]
    pub const fn load() -> Result<Option<SavedLogin>> {
        Ok(None)
    }

    pub fn save(_login: &SavedLogin) -> Result<()> {
        bail!("当前平台不支持 Windows 凭据管理器")
    }
}

#[cfg(test)]
mod tests {
    use super::SavedLogin;

    #[test]
    fn saved_login_json_round_trip() {
        let login = SavedLogin {
            server: "https://jellyfin.example.com/".to_owned(),
            username: "用户".to_owned(),
            password: "复杂密码".to_owned(),
        };
        let encoded = serde_json::to_vec(&login).unwrap();
        let decoded: SavedLogin = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.server, login.server);
        assert_eq!(decoded.username, login.username);
        assert_eq!(decoded.password, login.password);
    }
}
