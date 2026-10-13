use super::{MAX_BODY_BYTES, decode};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

// 외부 자격 증명과 캡처 본문 문자열을 같은 경계에서 가린다.
pub struct SummaryObserver {
    forbidden: Vec<String>,
}

impl SummaryObserver {
    pub fn new(forbidden: &[String]) -> Self {
        let mut forbidden: Vec<_> = forbidden
            .iter()
            .filter(|text| !text.is_empty())
            .cloned()
            .collect();
        forbidden.sort_by_key(|text| std::cmp::Reverse(text.len()));
        forbidden.dedup();
        Self { forbidden }
    }

    fn redact(&self, text: &str, limit: usize) -> String {
        let mut result = text.to_owned();
        for secret in &self.forbidden {
            result = result.replace(secret, "[redacted]");
        }
        // 알려진 값 외에도 토큰 모양의 단어는 출력하지 않는다.
        let mut bearer_value = false;
        result = result
            .split_inclusive(char::is_whitespace)
            .map(|part| {
                let word = part.trim();
                let after_bearer = bearer_value;
                bearer_value = word.eq_ignore_ascii_case("bearer");
                if after_bearer || token_pattern(word) {
                    "[redacted] ".to_owned()
                } else {
                    part.to_owned()
                }
            })
            .collect();
        result.chars().take(limit).collect()
    }

    fn identifier(&self, text: &str) -> Option<String> {
        // 기계용 필드에는 식별자만 허용하고 내용과 비밀 값은 생략한다.
        if self.forbidden.iter().any(|secret| secret == text) || token_pattern(text) {
            return None;
        }
        let first = text.as_bytes().first()?;
        if !(first.is_ascii_alphanumeric() || *first == b'_')
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return None;
        }
        Some(text[..text.len().min(100)].to_owned())
    }

    pub async fn summarize(&self, response: Response, account: &str) -> anyhow::Result<Value> {
        let (parts, body) = response.into_parts();
        let mut summary = json!({
            "status": parts.status.as_u16(), "account": account,
            "error": null, "usage": {}
        });
        let read = async {
            let mut stream = body.into_data_stream();
            let mut wire = Vec::new();
            let incremental = parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                })
                && matches!(super::encoding(&parts.headers)?.as_str(), "identity" | "");
            let mut parser = EventParser::default();
            let mut total = 0;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| anyhow::anyhow!("replay response read failed"))?;
                anyhow::ensure!(
                    chunk.len() <= MAX_BODY_BYTES.saturating_sub(total),
                    "replay response exceeds limit"
                );
                total += chunk.len();
                if incremental {
                    parser.push(&chunk, self, &mut summary)?;
                } else {
                    wire.extend_from_slice(&chunk);
                }
            }
            if incremental {
                parser.finish(self, &mut summary)?;
            }
            Ok::<_, anyhow::Error>((wire, incremental))
        };
        let (wire, incremental) = tokio::time::timeout(Duration::from_secs(120), read)
            .await
            .map_err(|_| anyhow::anyhow!("replay response timed out"))??;
        if incremental {
            return Ok(summary);
        }
        let decoded = decode(&parts.headers, &wire)?;
        if let Ok(value) = serde_json::from_slice::<Value>(&decoded) {
            self.observe(&value, &mut summary);
        } else {
            let mut parser = EventParser::default();
            parser.push(&decoded, self, &mut summary)?;
            parser.finish(self, &mut summary)?;
        }
        Ok(summary)
    }

    fn observe(&self, event: &Value, summary: &mut Value) {
        let root = event.get("response").unwrap_or(event);
        let error = root
            .get("error")
            .filter(|value| value.is_object())
            .or_else(|| {
                (event.get("type").and_then(Value::as_str) == Some("error")).then_some(event)
            });
        if let Some(error) = error {
            let mut safe = json!({"type": null, "code": null, "message": null});
            for (key, limit) in [("type", 100), ("code", 100), ("message", 200)] {
                if let Some(text) = error.get(key).and_then(Value::as_str) {
                    safe[key] = if key == "message" {
                        Value::String(self.redact(text, limit))
                    } else {
                        self.identifier(text)
                            .map(Value::String)
                            .unwrap_or(Value::Null)
                    };
                }
            }
            summary["error"] = safe;
        }
        for usage in [
            root.get("usage"),
            root.get("message").and_then(|message| message.get("usage")),
        ]
        .into_iter()
        .flatten()
        {
            for key in [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ] {
                if let Some(number) = usage.get(key).and_then(Value::as_u64) {
                    summary["usage"][key] = json!(number);
                }
            }
            if let Some(number) = usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
            {
                summary["usage"]["input_tokens_details"] = json!({"cached_tokens": number});
            }
        }
    }
}

fn token_pattern(text: &str) -> bool {
    text.contains("sk-")
        || text.contains("eyJ")
        || text
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case("bearer"))
}

// 줄 경계를 넘는 UTF-8과 CRLF를 보존하면서 이벤트 본문은 관찰 후 버린다.
#[derive(Default)]
struct EventParser {
    pending: Vec<u8>,
    data: String,
}

impl EventParser {
    fn push(
        &mut self,
        bytes: &[u8],
        observer: &SummaryObserver,
        summary: &mut Value,
    ) -> anyhow::Result<()> {
        self.pending.extend_from_slice(bytes);
        let mut start = 0;
        for end in 0..self.pending.len() {
            if self.pending[end] == b'\n' {
                let line = std::str::from_utf8(&self.pending[start..end])
                    .map_err(|_| anyhow::anyhow!("invalid replay response encoding"))?;
                Self::line(
                    line.trim_end_matches('\r'),
                    &mut self.data,
                    observer,
                    summary,
                )?;
                start = end + 1;
            }
        }
        self.pending.drain(..start);
        Ok(())
    }

    fn line(
        line: &str,
        data: &mut String,
        observer: &SummaryObserver,
        summary: &mut Value,
    ) -> anyhow::Result<()> {
        if line.is_empty() {
            if !data.is_empty() && data.trim() != "[DONE]" {
                let value = serde_json::from_str::<Value>(data)
                    .map_err(|_| anyhow::anyhow!("invalid replay response event"))?;
                observer.observe(&value, summary);
            }
            data.clear();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
        Ok(())
    }

    fn finish(&mut self, observer: &SummaryObserver, summary: &mut Value) -> anyhow::Result<()> {
        if !self.pending.is_empty() {
            let line = std::str::from_utf8(&self.pending)
                .map_err(|_| anyhow::anyhow!("invalid replay response encoding"))?;
            Self::line(
                line.trim_end_matches('\r'),
                &mut self.data,
                observer,
                summary,
            )?;
            self.pending.clear();
        }
        Self::line("", &mut self.data, observer, summary)
    }
}

pub async fn summarize(
    response: Response,
    account: &str,
    forbidden: &[String],
) -> anyhow::Result<Value> {
    SummaryObserver::new(forbidden)
        .summarize(response, account)
        .await
}
