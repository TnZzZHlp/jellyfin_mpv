use std::{env, fs, path::PathBuf, sync::RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const ALL_SEGMENT_TYPES: [&str; 5] = ["Intro", "Outro", "Recap", "Preview", "Commercial"];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SkipSettings {
    pub intro: bool,
    pub outro: bool,
    pub recap: bool,
    pub preview: bool,
    pub commercial: bool,
}

impl Default for SkipSettings {
    fn default() -> Self {
        Self {
            intro: true,
            outro: true,
            recap: false,
            preview: false,
            commercial: false,
        }
    }
}

impl SkipSettings {
    pub fn supports(segment_type: &str) -> bool {
        matches!(
            segment_type,
            "Intro" | "Outro" | "Recap" | "Preview" | "Commercial"
        )
    }

    pub fn is_enabled(&self, segment_type: &str) -> bool {
        match segment_type {
            "Intro" => self.intro,
            "Outro" => self.outro,
            "Recap" => self.recap,
            "Preview" => self.preview,
            "Commercial" => self.commercial,
            _ => false,
        }
    }
}

pub struct SkipSettingsStore {
    path: PathBuf,
    current: RwLock<SkipSettings>,
}

impl SkipSettingsStore {
    pub fn load() -> Result<Self> {
        Self::load_from(settings_path())
    }

    pub fn default_store() -> Self {
        Self::new(settings_path(), SkipSettings::default())
    }

    pub fn snapshot(&self) -> SkipSettings {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn update(&self, settings: SkipSettings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建设置目录 {} 失败", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(&settings).context("序列化跳过设置失败")?;
        fs::write(&self.path, bytes)
            .with_context(|| format!("写入跳过设置 {} 失败", self.path.display()))?;
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
        Ok(())
    }

    fn load_from(path: PathBuf) -> Result<Self> {
        let current = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("解析跳过设置 {} 失败", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SkipSettings::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("读取跳过设置 {} 失败", path.display()));
            }
        };
        Ok(Self::new(path, current))
    }

    fn new(path: PathBuf, current: SkipSettings) -> Self {
        Self {
            path,
            current: RwLock::new(current),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, current: SkipSettings) -> Self {
        Self::new(path, current)
    }
}

fn settings_path() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    base.join("jellyfin_mpv").join("settings.json")
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn defaults_to_skipping_intro_and_outro_only() {
        let settings = SkipSettings::default();
        assert!(settings.is_enabled("Intro"));
        assert!(settings.is_enabled("Outro"));
        assert!(!settings.is_enabled("Recap"));
        assert!(!settings.is_enabled("Preview"));
        assert!(!settings.is_enabled("Commercial"));
        assert!(!settings.is_enabled("Unknown"));
    }

    #[test]
    fn persists_and_loads_all_segment_choices() {
        let directory = env::temp_dir().join(format!("jellyfin-mpv-settings-{}", Uuid::new_v4()));
        let path = directory.join("settings.json");
        let store = SkipSettingsStore::for_test(path.clone(), SkipSettings::default());
        let expected = SkipSettings {
            intro: false,
            outro: true,
            recap: true,
            preview: true,
            commercial: true,
        };

        store.update(expected.clone()).unwrap();
        let loaded = SkipSettingsStore::load_from(path).unwrap();
        assert_eq!(loaded.snapshot(), expected);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn supports_every_native_jellyfin_segment_type() {
        for segment_type in ALL_SEGMENT_TYPES {
            assert!(SkipSettings::supports(segment_type));
        }
        assert!(!SkipSettings::supports("Unknown"));
    }
}
