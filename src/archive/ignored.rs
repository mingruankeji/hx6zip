use std::{fmt::Write as _, fs, path::Path};

use anyhow::{Context, Result};

pub(super) const REPORT_NAME: &str = "Ignored_Items.mmzip";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecordKind {
    IgnoredDirectory,
    IgnoredFile,
    Error,
}

impl RecordKind {
    fn label(self) -> &'static str {
        match self {
            Self::IgnoredDirectory => "忽略文件夹",
            Self::IgnoredFile => "忽略文件",
            Self::Error => "错误事件",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Record {
    pub kind: RecordKind,
    pub path: Option<String>,
    pub name: Option<String>,
    pub reason: String,
}

pub(super) fn write(path: &Path, records: &[Record]) -> Result<()> {
    let mut contents = String::from("Hx6Zip 忽略项与错误记录\n");
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            contents.push_str("\n--------------------\n");
        }
        writeln!(&mut contents, "类型：{}", record.kind.label()).expect("写入 String 不会失败");
        if let Some(path) = &record.path {
            writeln!(&mut contents, "路径：{}", escape_line(path)).expect("写入 String 不会失败");
        }
        if let Some(name) = &record.name {
            writeln!(&mut contents, "名称：{}", escape_line(name)).expect("写入 String 不会失败");
        }
        writeln!(&mut contents, "说明：{}", escape_line(&record.reason))
            .expect("写入 String 不会失败");
    }
    fs::write(path, contents).with_context(|| format!("无法保存忽略项记录 {}", path.display()))
}

fn escape_line(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                write!(&mut escaped, "\\u{{{:x}}}", character as u32)
                    .expect("写入 String 不会失败");
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_human_readable_text_and_escapes_line_breaks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(REPORT_NAME);
        write(
            &path,
            &[
                Record {
                    kind: RecordKind::IgnoredDirectory,
                    path: Some("C:\\项目\\cache".to_owned()),
                    name: Some("项目/cache".to_owned()),
                    reason: "包含 CACHEDIR.TAG".to_owned(),
                },
                Record {
                    kind: RecordKind::Error,
                    path: Some("C:\\项目\\bad\nfile".to_owned()),
                    name: Some("bad\nfile".to_owned()),
                    reason: "读取失败".to_owned(),
                },
            ],
        )
        .unwrap();

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.starts_with("Hx6Zip 忽略项与错误记录\n"));
        assert!(contents.contains("类型：忽略文件夹\n"));
        assert!(contents.contains("路径：C:\\项目\\cache\n"));
        assert!(contents.contains("类型：错误事件\n"));
        assert!(contents.contains("名称：bad\\nfile\n"));
        assert!(!contents.starts_with('{'));
    }
}
