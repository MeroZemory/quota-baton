//! 합성 대화의 본문 캡처와 재생 준비를 담당한다. 인증과 전송은 릴레이가 맡는다.

use crate::accounts::Provider;
use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

mod summary;
pub use summary::{SummaryObserver, summarize};

pub const DEFAULT_CAPTURE_LIMIT: usize = 20;
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CAPTURE_FILE_BYTES: usize = 192 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StripMode {
    #[default]
    None,
    Reasoning,
    All,
}

// Debug를 구현하지 않아 호출자가 실수로 대화 본문을 로그에 남기지 않게 한다.
pub struct PreparedReplay {
    pub route: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub presence: Value,
    pub forbidden: Vec<String>,
    pub diagnostics: Value,
}

#[derive(Serialize, Deserialize)]
struct CaptureFile {
    route: String,
    headers: Vec<(String, String)>,
    account_id: String,
    raw_body_base64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoded_body: Option<Value>,
}

pub struct BodyCapture {
    directory: PathBuf,
    remaining: AtomicUsize,
}

fn sandbox_path(path: &Path, directory: bool) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("capture path must exist"))?;
    let boundary = if directory {
        canonical.as_path()
    } else {
        canonical
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid capture path"))?
    };
    anyhow::ensure!(
        boundary
            .components()
            .any(|part| matches!(part, Component::Normal(name) if name == ".sandbox"))
            && if directory {
                canonical.is_dir()
            } else {
                canonical.is_file()
            },
        "capture path must be inside a canonical .sandbox directory"
    );
    Ok(canonical)
}

fn allowed_route(route: &str) -> bool {
    matches!(route, "/v1/messages" | "/v1/responses")
}

fn secret_header(name: &str) -> bool {
    ["authorization", "x-api-key", "cookie", "host"]
        .iter()
        .any(|secret| name.eq_ignore_ascii_case(secret))
}

impl BodyCapture {
    pub fn new(dir: PathBuf, limit: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(limit > 0, "capture limit must be positive");
        Ok(Self {
            directory: sandbox_path(&dir, true)?,
            remaining: AtomicUsize::new(limit),
        })
    }

    pub fn record(
        &self,
        route: &str,
        headers: &HeaderMap,
        account_id: &str,
        raw_body: &[u8],
    ) -> anyhow::Result<Option<PathBuf>> {
        self.record_file(route, headers, account_id, raw_body)
    }

    fn record_file(
        &self,
        route: &str,
        headers: &HeaderMap,
        account_id: &str,
        raw_body: &[u8],
    ) -> anyhow::Result<Option<PathBuf>> {
        if !allowed_route(route) || self.remaining.load(Ordering::Relaxed) == 0 {
            return Ok(None);
        }
        anyhow::ensure!(
            raw_body.len() <= MAX_BODY_BYTES,
            "capture body exceeds limit"
        );
        let decoded_body = if encoding(headers)? == "zstd" {
            let decoded = decode(headers, raw_body)?;
            Some(
                serde_json::from_slice(&decoded)
                    .map_err(|_| anyhow::anyhow!("capture body is not JSON"))?,
            )
        } else {
            None
        };
        let mut saved_headers = Vec::new();
        for (name, value) in headers {
            if !secret_header(name.as_str()) {
                saved_headers.push((
                    name.to_string(),
                    value
                        .to_str()
                        .map_err(|_| anyhow::anyhow!("capture header is not text"))?
                        .to_owned(),
                ));
            }
        }
        let capture = CaptureFile {
            route: route.to_owned(),
            headers: saved_headers,
            account_id: account_id.to_owned(),
            raw_body_base64: STANDARD.encode(raw_body),
            decoded_body,
        };
        let serialized = serde_json::to_vec(&capture)
            .map_err(|_| anyhow::anyhow!("capture serialization failed"))?;
        anyhow::ensure!(
            serialized.len() <= MAX_CAPTURE_FILE_BYTES,
            "capture file exceeds limit"
        );
        // 생성 직전에도 실제 경로를 확인하고 기존 파일은 덮어쓰지 않는다.
        let destination = sandbox_path(&self.directory, true)?
            .join(format!("body-{:032x}.json", rand::random::<u128>()));
        if self
            .remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_err()
        {
            return Ok(None);
        }
        let write = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            if let Err(error) = file.write_all(&serialized) {
                drop(file);
                let _ = std::fs::remove_file(&destination);
                return Err(error);
            }
            Ok::<_, std::io::Error>(())
        })();
        if write.is_err() {
            self.remaining.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("capture file write failed");
        }
        Ok(Some(destination))
    }
}

pub(crate) fn bounded_read(reader: impl Read) -> anyhow::Result<Vec<u8>> {
    bounded_read_limit(reader, MAX_BODY_BYTES)
}

fn bounded_read_limit(reader: impl Read, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("body decoding failed"))?;
    anyhow::ensure!(bytes.len() <= limit, "decoded body exceeds limit");
    Ok(bytes)
}

fn encoding(headers: &HeaderMap) -> anyhow::Result<String> {
    let mut values = headers.get_all("content-encoding").iter();
    let result = values
        .next()
        .map(|v| v.to_str())
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid content encoding"))?
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    anyhow::ensure!(
        values.next().is_none(),
        "multiple content encodings are unsupported"
    );
    Ok(result)
}

pub(crate) fn decode(headers: &HeaderMap, raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(raw.len() <= MAX_BODY_BYTES, "raw body exceeds limit");
    match encoding(headers)?.as_str() {
        "identity" | "" => Ok(raw.to_vec()),
        "zstd" => {
            let mut decoder = zstd::stream::read::Decoder::new(raw)
                .map_err(|_| anyhow::anyhow!("body decoding failed"))?;
            decoder
                .window_log_max(23)
                .map_err(|_| anyhow::anyhow!("body decoding failed"))?;
            bounded_read(decoder)
        }
        "gzip" => bounded_read(flate2::read::GzDecoder::new(raw)),
        "deflate" => bounded_read(flate2::read::ZlibDecoder::new(raw)),
        "br" => {
            // 하위 7비트의 확장 창 표식을 디코더 생성 전에 거부한다.
            anyhow::ensure!(
                raw.first().is_none_or(|byte| byte & 0x7f != 0x11),
                "brotli large windows are unsupported"
            );
            bounded_read(brotli::Decompressor::new(raw, 4096))
        }
        _ => anyhow::bail!("unsupported content encoding"),
    }
}

fn encode(headers: &HeaderMap, plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let encoded = match encoding(headers)?.as_str() {
        "identity" | "" => return Ok(plain.to_vec()),
        "zstd" => zstd::stream::encode_all(plain, 0),
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(plain).and_then(|()| encoder.finish())
        }
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(plain).and_then(|()| encoder.finish())
        }
        "br" => {
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
            encoder.write_all(plain).map(|()| encoder.into_inner())
        }
        _ => anyhow::bail!("changing this content encoding is unsupported"),
    };
    encoded.map_err(|_| anyhow::anyhow!("replay encoding failed"))
}

pub fn prepare(
    capture_path: &Path,
    account_provider: Provider,
    strip: StripMode,
    tamper: bool,
) -> anyhow::Result<PreparedReplay> {
    let path = sandbox_path(capture_path, false)?;
    let file = File::open(path).map_err(|_| anyhow::anyhow!("capture file read failed"))?;
    anyhow::ensure!(
        file.metadata()
            .map_err(|_| anyhow::anyhow!("capture file read failed"))?
            .len()
            <= MAX_CAPTURE_FILE_BYTES as u64,
        "capture file exceeds limit"
    );
    let saved: CaptureFile =
        serde_json::from_slice(&bounded_read_limit(file, MAX_CAPTURE_FILE_BYTES)?)
            .map_err(|_| anyhow::anyhow!("invalid capture file"))?;
    anyhow::ensure!(allowed_route(&saved.route), "unsupported capture route");
    anyhow::ensure!(
        matches!(
            (account_provider, saved.route.as_str()),
            (Provider::Claude, "/v1/messages") | (Provider::Codex, "/v1/responses")
        ),
        "capture route does not match account provider"
    );
    let mut headers = HeaderMap::new();
    for (name, value) in saved.headers {
        if secret_header(&name) {
            continue;
        }
        let name = axum::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid capture header"))?;
        let value = axum::http::HeaderValue::from_str(&value)
            .map_err(|_| anyhow::anyhow!("invalid capture header"))?;
        headers.append(name, value);
    }
    let raw = STANDARD
        .decode(saved.raw_body_base64)
        .map_err(|_| anyhow::anyhow!("invalid capture body encoding"))?;
    let decoded = decode(&headers, &raw)?;
    let mut body: Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow::anyhow!("capture body is not JSON"))?;
    let presence = presence(&body, account_provider);
    let mut forbidden = vec![String::from_utf8_lossy(&decoded).into_owned()];
    collect_strings(&body, &mut forbidden);
    let (removed, removed_counts) = strip_body(&mut body, account_provider, strip);
    let tampered_type = if tamper {
        tamper_body(&mut body, account_provider)
    } else {
        None
    };
    let diagnostics = json!({"strip_encrypted":strip,"removed_counts":removed_counts,"tampered_type":tampered_type});
    let changed = removed || tampered_type.is_some();
    let wire = if changed {
        let plain = serde_json::to_vec(&body)
            .map_err(|_| anyhow::anyhow!("replay serialization failed"))?;
        anyhow::ensure!(plain.len() <= MAX_BODY_BYTES, "replay body exceeds limit");
        let wire = encode(&headers, &plain)?;
        anyhow::ensure!(wire.len() <= MAX_BODY_BYTES, "replay body exceeds limit");
        // 길이가 바뀐 본문의 전송 길이는 릴레이가 다시 결정한다.
        headers.remove("content-length");
        wire
    } else {
        raw
    };
    collect_strings(&body, &mut forbidden);
    forbidden.sort();
    forbidden.dedup();
    Ok(PreparedReplay {
        route: saved.route,
        headers,
        body: wire.into(),
        presence,
        forbidden,
        diagnostics,
    })
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(text) if !text.is_empty() => strings.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, strings)),
        Value::Object(map) => map
            .iter()
            // 역할과 표준 타입 이름은 대화 내용이 아니다. 알 수 없는 값은 가린다.
            .filter(|(key, value)| {
                !(matches!(key.as_str(), "type" | "role")
                    && matches!(
                        value.as_str(),
                        Some(
                            "reasoning"
                                | "compaction"
                                | "message"
                                | "thinking"
                                | "redacted_thinking"
                                | "text"
                                | "input_text"
                                | "output_text"
                                | "input_image"
                                | "user"
                                | "assistant"
                                | "system"
                                | "developer"
                                | "tool"
                        )
                    ))
            })
            .for_each(|(_, item)| collect_strings(item, strings)),
        _ => {}
    }
}

fn presence(body: &Value, provider: Provider) -> Value {
    match provider {
        Provider::Codex => {
            let mut reasoning = 0;
            let mut compaction = 0;
            let mut message_parts = 0;
            if let Some(input) = body.get("input").and_then(Value::as_array) {
                for item in input {
                    let count = count_field(item, "encrypted_content");
                    match item.get("type").and_then(Value::as_str) {
                        Some("reasoning") => reasoning += count,
                        Some("compaction") => compaction += count,
                        Some("message") => message_parts += count,
                        None if item.get("role").is_some() => message_parts += count,
                        _ => {}
                    }
                }
            }
            json!({"provider": "codex", "encrypted_content": {"reasoning": reasoning, "compaction": compaction, "message_parts": message_parts}})
        }
        Provider::Claude => {
            let signed = body
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|message| message.get("content").and_then(Value::as_array))
                .flatten()
                .filter(|block| {
                    block.get("type").and_then(Value::as_str) == Some("thinking")
                        && block.get("signature").and_then(Value::as_str).is_some()
                })
                .count();
            json!({"provider": "claude", "signed_thinking": signed})
        }
    }
}

fn count_field(value: &Value, name: &str) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.get(name).is_some_and(Value::is_string))
                + map.values().map(|v| count_field(v, name)).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(|v| count_field(v, name)).sum(),
        _ => 0,
    }
}

fn item_type(item: &Value) -> &'static str {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => "reasoning",
        Some("compaction") => "compaction",
        Some("message") => "message",
        None if item.get("role").is_some() => "message",
        _ => "other",
    }
}

fn strip_body(body: &mut Value, provider: Provider, mode: StripMode) -> (bool, Value) {
    let mut counts = match provider {
        Provider::Codex => json!({"reasoning":0,"compaction":0,"message":0,"other":0}),
        Provider::Claude => json!({"thinking":0,"redacted_thinking":0}),
    };
    if mode == StripMode::None {
        return (false, counts);
    }
    match provider {
        Provider::Codex => {
            let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
                return (false, counts);
            };
            let before = input.len();
            input.retain(|item| {
                let remove = match mode {
                    StripMode::None => false,
                    StripMode::Reasoning => {
                        item_type(item) == "reasoning" && item.get("encrypted_content").is_some()
                    }
                    StripMode::All => has_field(item, "encrypted_content"),
                };
                if remove {
                    let key = item_type(item);
                    counts[key] = json!(counts[key].as_u64().unwrap_or(0) + 1);
                }
                !remove
            });
            (before != input.len(), counts)
        }
        Provider::Claude => {
            let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
                return (false, counts);
            };
            let last = messages.iter().rposition(|message| {
                message.get("role").and_then(Value::as_str) == Some("assistant")
            });
            let start = last.map(|mut index| {
                while index > 0
                    && messages[index - 1].get("role").and_then(Value::as_str) == Some("assistant")
                {
                    index -= 1;
                }
                index
            });
            let mut changed = false;
            for (index, message) in messages.iter_mut().enumerate() {
                if mode == StripMode::Reasoning
                    && start
                        .zip(last)
                        .is_some_and(|(start, end)| (start..=end).contains(&index))
                {
                    continue;
                }
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                    let before = blocks.len();
                    blocks.retain(|block| {
                        if let Some(key @ ("thinking" | "redacted_thinking")) =
                            block.get("type").and_then(Value::as_str)
                        {
                            counts[key] = json!(counts[key].as_u64().unwrap_or(0) + 1);
                            false
                        } else {
                            true
                        }
                    });
                    changed |= before != blocks.len();
                }
            }
            (changed, counts)
        }
    }
}

fn has_field(value: &Value, name: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(name) || map.values().any(|value| has_field(value, name))
        }
        Value::Array(items) => items.iter().any(|value| has_field(value, name)),
        _ => false,
    }
}

fn flip_first(value: &mut Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get_mut(key)
                && let Some(first) = text.chars().next()
            {
                text.replace_range(..first.len_utf8(), if first == 'A' { "B" } else { "A" });
                return true;
            }
            map.values_mut().any(|value| flip_first(value, key))
        }
        Value::Array(items) => items.iter_mut().any(|value| flip_first(value, key)),
        _ => false,
    }
}

fn tamper_body(body: &mut Value, provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Codex => {
            for item in body.get_mut("input").and_then(Value::as_array_mut)? {
                let kind = item_type(item);
                if flip_first(item, "encrypted_content") {
                    return Some(kind);
                }
            }
            None
        }
        Provider::Claude => {
            for message in body.get_mut("messages").and_then(Value::as_array_mut)? {
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in blocks {
                        let kind = match block.get("type").and_then(Value::as_str) {
                            Some("thinking") => "thinking",
                            Some("redacted_thinking") => "redacted_thinking",
                            _ => "other",
                        };
                        if flip_first(block, "signature") {
                            return Some(kind);
                        }
                    }
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests;
