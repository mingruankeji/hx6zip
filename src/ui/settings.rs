use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use directories::BaseDirs;
use mzip::archive::IntegrityMode;
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
}

pub(super) fn settings_path() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.data_local_dir().join("MZip").join("settings.json"))
}

/// Missing or unreadable settings fall back to fast mode.
pub(super) fn load_integrity_mode(path: Option<&Path>) -> IntegrityMode {
    let stored = path
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<StoredSettings>(&bytes).ok())
        .unwrap_or_default();
    match stored.integrity_mode {
        StoredMode::Safe => IntegrityMode::Safe,
        StoredMode::Normal => IntegrityMode::Normal,
    }
}

pub(super) fn save_integrity_mode(path: Option<&Path>, mode: IntegrityMode) -> Result<()> {
    let path = path.context("无法确定设置文件位置")?;
    let settings = StoredSettings {
        integrity_mode: match mode {
            IntegrityMode::Safe => StoredMode::Safe,
            IntegrityMode::Normal => StoredMode::Normal,
        },
    };
    let parent = path.parent().context("设置文件路径无效")?;
    fs::create_dir_all(parent).context("无法创建设置文件夹")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("无法保存设置")?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), &settings).context("无法保存设置")?;
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
    fn integrity_mode_defaults_to_fast_and_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("settings.json");
        assert_eq!(load_integrity_mode(Some(&path)), IntegrityMode::Normal);
        assert_eq!(load_integrity_mode(None), IntegrityMode::Normal);

        save_integrity_mode(Some(&path), IntegrityMode::Normal).unwrap();
        assert_eq!(load_integrity_mode(Some(&path)), IntegrityMode::Normal);
        save_integrity_mode(Some(&path), IntegrityMode::Safe).unwrap();
        assert_eq!(load_integrity_mode(Some(&path)), IntegrityMode::Safe);

        fs::write(&path, b"not json").unwrap();
        assert_eq!(load_integrity_mode(Some(&path)), IntegrityMode::Normal);
    }
}
