use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use directories::BaseDirs;
use hx6zip::archive::{IntegrityMode, SourceFilters};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredMode {
    Safe,
    #[default]
    Normal,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct StoredSettings {
    integrity_mode: StoredMode,
    ignore_common_cache_dirs: bool,
    ignore_frontend_dependencies: bool,
    ignore_version_control_dirs: bool,
    ignore_locked_files: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Settings {
    pub(super) integrity_mode: IntegrityMode,
    pub(super) source_filters: SourceFilters,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            integrity_mode: IntegrityMode::Normal,
            source_filters: SourceFilters::default(),
        }
    }
}

pub(super) fn settings_path() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.data_local_dir().join("Hx6Zip").join("settings.json"))
}

/// Missing or unreadable settings fall back to the existing defaults.
pub(super) fn load(path: Option<&Path>) -> Settings {
    let stored = path
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<StoredSettings>(&bytes).ok())
        .unwrap_or_default();
    Settings {
        integrity_mode: match stored.integrity_mode {
            StoredMode::Safe => IntegrityMode::Safe,
            StoredMode::Normal => IntegrityMode::Normal,
        },
        source_filters: SourceFilters {
            ignore_common_cache_dirs: stored.ignore_common_cache_dirs,
            ignore_frontend_dependencies: stored.ignore_frontend_dependencies,
            ignore_version_control_dirs: stored.ignore_version_control_dirs,
            ignore_locked_files: stored.ignore_locked_files,
        },
    }
}

pub(super) fn save(path: Option<&Path>, settings: Settings) -> Result<()> {
    let path = path.context("无法确定设置文件位置")?;
    let stored = StoredSettings {
        integrity_mode: match settings.integrity_mode {
            IntegrityMode::Safe => StoredMode::Safe,
            IntegrityMode::Normal => StoredMode::Normal,
        },
        ignore_common_cache_dirs: settings.source_filters.ignore_common_cache_dirs,
        ignore_frontend_dependencies: settings.source_filters.ignore_frontend_dependencies,
        ignore_version_control_dirs: settings.source_filters.ignore_version_control_dirs,
        ignore_locked_files: settings.source_filters.ignore_locked_files,
    };
    let parent = path.parent().context("设置文件路径无效")?;
    fs::create_dir_all(parent).context("无法创建设置文件夹")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("无法保存设置")?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), &stored).context("无法保存设置")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("无法保存设置")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_and_round_trips_all_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("settings.json");
        assert_eq!(load(Some(&path)), Settings::default());
        assert_eq!(load(None), Settings::default());

        let settings = Settings {
            integrity_mode: IntegrityMode::Safe,
            source_filters: SourceFilters {
                ignore_common_cache_dirs: true,
                ignore_frontend_dependencies: true,
                ignore_version_control_dirs: true,
                ignore_locked_files: true,
            },
        };
        save(Some(&path), settings).unwrap();
        assert_eq!(load(Some(&path)), settings);

        fs::write(&path, b"not json").unwrap();
        assert_eq!(load(Some(&path)), Settings::default());
    }

    #[test]
    fn old_settings_keep_their_defaults_for_new_options() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, br#"{"integrity_mode":"safe"}"#).unwrap();

        assert_eq!(
            load(Some(&path)),
            Settings {
                integrity_mode: IntegrityMode::Safe,
                source_filters: SourceFilters::default(),
            }
        );
    }
}
