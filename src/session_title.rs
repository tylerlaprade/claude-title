use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionTitle {
    path: PathBuf,
    identity: (u64, u64),
    offset: u64,
    custom_title: Option<String>,
    agent_setting: Option<String>,
}

impl SessionTitle {
    pub fn refresh(&mut self, path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        let identity = (metadata.dev(), metadata.ino());
        if self.path != path || self.identity != identity || self.offset > metadata.len() {
            *self = Self {
                path: path.to_path_buf(),
                identity,
                ..Self::default()
            };
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut line = Vec::new();
        loop {
            line.clear();
            let bytes = reader.read_until(b'\n', &mut line).ok()?;
            if bytes == 0 || !line.ends_with(b"\n") {
                break;
            }
            self.offset += bytes as u64;
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            if !text.contains("custom-title") && !text.contains("agent-setting") {
                continue;
            }
            let Ok(record) = serde_json::from_str::<TitleRecord>(text) else {
                continue;
            };
            match record.kind.as_str() {
                "custom-title" => self.custom_title = record.custom_title,
                "agent-setting" => self.agent_setting = record.agent_setting,
                _ => {}
            }
        }
        self.custom_title
            .clone()
            .or_else(|| self.agent_setting.clone())
            .filter(|title| !title.is_empty())
    }
}

#[derive(Deserialize)]
struct TitleRecord {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "agentSetting")]
    agent_setting: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    #[test]
    fn incremental_scan_keeps_a_rename_and_retries_partial_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            "{\"type\": \"custom-title\", \"customTitle\": \"Chosen\"}\n",
        )
        .unwrap();
        let mut scan = SessionTitle::default();
        assert_eq!(scan.refresh(&path).as_deref(), Some("Chosen"));
        let offset = scan.offset;
        assert_eq!(scan.refresh(&path).as_deref(), Some("Chosen"));
        assert_eq!(scan.offset, offset);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"agent-setting\",\"agentSetting\":\"Automatic\"}\n{\"type\":\"custom-title\",\"customTitle\":\"Next").unwrap();
        assert_eq!(scan.refresh(&path).as_deref(), Some("Chosen"));
        file.write_all(b"\"}\n").unwrap();
        assert_eq!(scan.refresh(&path).as_deref(), Some("Next"));
    }

    #[test]
    fn replacing_or_truncating_the_transcript_resets_the_scan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        let replacement = directory.path().join("replacement.jsonl");
        fs::write(
            &path,
            "{\"type\":\"custom-title\",\"customTitle\":\"Original\"}\n",
        )
        .unwrap();
        let mut scan = SessionTitle::default();
        assert_eq!(scan.refresh(&path).as_deref(), Some("Original"));
        fs::write(
            &replacement,
            "{\"type\":\"custom-title\",\"customTitle\":\"Replacement\"}\n",
        )
        .unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert_eq!(scan.refresh(&path).as_deref(), Some("Replacement"));
        fs::write(
            &path,
            "{\"type\":\"custom-title\",\"customTitle\":\"New\"}\n",
        )
        .unwrap();
        assert_eq!(scan.refresh(&path).as_deref(), Some("New"));
    }
}
