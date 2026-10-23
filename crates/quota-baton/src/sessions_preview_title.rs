//! Best-effort preview titles; never retain or log rollout contents.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value;

const READ_LIMIT: u64 = 4 * 1024 * 1024;

/// Presentation owns whitespace/control sanitizing and display-width truncation.
pub(crate) fn resolve(
    db_title: Option<&str>,
    index_name: Option<&str>,
    path: Option<&Path>,
) -> String {
    db_title
        .and_then(first_line)
        .or_else(|| index_name.and_then(first_line))
        .map(str::to_owned)
        .or_else(|| path.and_then(rollout_title))
        .unwrap_or_default()
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().find(|line| !line.trim().is_empty())
}

fn rollout_title(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    if path.extension().is_some_and(|extension| extension == "zst") {
        // Bound compressed input and window memory as well as decoded output.
        let mut decoder = zstd::stream::read::Decoder::new(file.take(READ_LIMIT)).ok()?;
        decoder.window_log_max(23).ok()?;
        read_title(decoder)
    } else {
        read_title(file)
    }
}

fn read_title(reader: impl Read) -> Option<String> {
    let mut reader = BufReader::new(reader.take(READ_LIMIT));
    let mut line = Vec::new();
    let mut consumed = 0;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line).ok()?;
        consumed += count as u64;
        if count == 0 || (consumed == READ_LIMIT && !line.ends_with(b"\n")) {
            // Do not interpret a partial record at the artificial EOF.
            return None;
        }
        let record: Value = serde_json::from_slice(&line).ok()?;
        let payload = record.get("payload");
        match record.get("type").and_then(Value::as_str) {
            Some("event_msg")
                if payload?.get("type").and_then(Value::as_str) == Some("user_message") =>
            {
                return payload?
                    .get("message")?
                    .as_str()
                    .and_then(first_line)
                    .map(str::to_owned);
            }
            Some("response_item")
                if payload?.get("role").and_then(Value::as_str) == Some("user") =>
            {
                // The first user message owns the fallback even if it is empty.
                return payload?
                    .get("content")?
                    .as_array()?
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("input_text" | "text")
                        )
                    })
                    .find_map(|item| item.get("text")?.as_str().and_then(first_line))
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(bytes: &[u8], compressed: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9.6-preview-title-tests");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::tempdir_in(root).unwrap();
        let path = directory.path().join(if compressed {
            "rollout.jsonl.zst"
        } else {
            "rollout.jsonl"
        });
        let bytes = if compressed {
            zstd::stream::encode_all(bytes, 1).unwrap()
        } else {
            bytes.to_vec()
        };
        fs::write(&path, bytes).unwrap();
        (directory, path)
    }

    fn message(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"event_msg", "payload":{
                "type":"user_message", "message":text
            }})
        )
    }

    #[test]
    fn title_resolution_prefer_db() {
        assert_eq!(resolve(Some("DB title"), Some("index"), None), "DB title");
        assert_eq!(
            resolve(Some("\n  DB title\t\nsecond"), None, None),
            "  DB title\t"
        );
    }

    #[test]
    fn title_resolution_prefer_index() {
        assert_eq!(resolve(Some(" \n\t"), Some("index"), None), "index");
        let (_directory, path) = fixture(message("prompt").as_bytes(), false);
        assert_eq!(resolve(None, Some("index"), Some(&path)), "index");
    }

    #[test]
    fn title_resolution_multiline_prompt() {
        let body = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{}}}}\n{}{}",
            message("\n \n  첫 줄\t\nsecond line"),
            message("later prompt")
        );
        let (_directory, path) = fixture(body.as_bytes(), false);
        assert_eq!(resolve(None, Some(" "), Some(&path)), "  첫 줄\t");
    }

    #[test]
    fn title_resolution_response_item() {
        let body = serde_json::json!({"type":"response_item","payload":{
            "role":"user","content":[
                {"type":"input_image"},
                {"type":"input_text","text":"\n "},
                {"type":"text","text":"\nresponse title\nsecond"}
            ]
        }})
        .to_string();
        let (_directory, path) = fixture(body.as_bytes(), false);
        assert_eq!(resolve(None, None, Some(&path)), "response title");
    }

    #[test]
    fn title_resolution_compressed_bound() {
        let (_directory, path) = fixture(message("compressed title").as_bytes(), true);
        assert_eq!(resolve(None, None, Some(&path)), "compressed title");

        // A highly compressible over-limit record must not expose a later title.
        let mut body = vec![b' '; READ_LIMIT as usize];
        body.push(b'\n');
        body.extend_from_slice(message("beyond bound").as_bytes());
        for compressed in [false, true] {
            let (_directory, path) = fixture(&body, compressed);
            assert_eq!(resolve(None, None, Some(&path)), "");
        }

        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer.fill(b' ');
                Ok(buffer.len())
            }
        }
        // Also proves termination without relying on a finite file's EOF.
        assert_eq!(read_title(Endless), None);
    }

    #[test]
    fn title_resolution_failure_and_empty_first_message() {
        assert_eq!(resolve(None, None, None), "");
        for body in [
            b"invalid JSON\n".to_vec(),
            [message(" \n\t"), message("later")].concat().into_bytes(),
        ] {
            let (_directory, path) = fixture(&body, false);
            assert_eq!(resolve(None, None, Some(&path)), "");
        }
        let (_directory, path) = fixture(b"invalid zstd", true);
        fs::write(&path, b"invalid zstd").unwrap();
        assert_eq!(resolve(None, None, Some(&path)), "");
        fs::remove_file(&path).unwrap();
        assert_eq!(resolve(None, None, Some(&path)), "");
    }
}
