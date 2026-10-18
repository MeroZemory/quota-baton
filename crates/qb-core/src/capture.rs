#[cfg(all(feature = "shape-capture", not(debug_assertions)))]
compile_error!("shape-capture requires debug_assertions; release capture is forbidden");

#[cfg(all(feature = "shape-capture", debug_assertions))]
mod enabled {
    use crate::observability::{CaptureOptions, ua_product};
    use axum::http::HeaderMap;
    use serde::Deserialize;
    use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::OpenOptions,
        io::{BufReader, Read},
        path::{Component, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    const MAX_BODY: usize = 64 * 1024 * 1024;
    const MAX_KEYS: usize = 4096;

    pub(crate) fn response_shape(headers: &HeaderMap) -> Value {
        let mut values = BTreeMap::new();
        for name in headers.keys().filter(|name| {
            name.as_str().starts_with("anthropic-ratelimit-")
                || name.as_str().starts_with("x-codex-")
                || matches!(name.as_str(), "retry-after" | "x-request-id")
        }) {
            let entries: Vec<_> = headers
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .filter(|value| value.len() <= 4096)
                .collect();
            values.insert(name.as_str(), entries);
        }
        json!({"response_header_names": values.keys().collect::<Vec<_>>(), "response_header_values": values})
    }

    pub struct Capture {
        destination: PathBuf,
        remaining: AtomicUsize,
    }
    impl Capture {
        pub fn new(options: CaptureOptions) -> anyhow::Result<Self> {
            let dir = options
                .dir
                .canonicalize()
                .map_err(|_| anyhow::anyhow!("capture directory must already exist"))?;
            anyhow::ensure!(
                dir.is_dir()
                    && dir
                        .components()
                        .any(|c| matches!(c, Component::Normal(v) if v == ".sandbox")),
                "capture directory must be inside a canonical .sandbox directory"
            );
            anyhow::ensure!(options.limit > 0, "capture limit must be positive");
            let destination = dir.join(format!("shape-{:032x}.jsonl", rand::random::<u128>()));
            // 명시적으로 검증한 디렉터리에 실행별 파일만 만든다.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|_| anyhow::anyhow!("capture file creation failed"))?;
            Ok(Self {
                destination,
                remaining: AtomicUsize::new(options.limit),
            })
        }

        pub fn shape(&self, headers: &HeaderMap, body: &[u8]) -> Option<Value> {
            self.remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .ok()?;
            let names: BTreeSet<_> = headers
                .keys()
                .map(|h| h.as_str())
                .filter(|name| crate::dashboard::labels::capture_header_allowed(name))
                .collect();
            let mut values = BTreeMap::new();
            for name in [
                "anthropic-beta",
                "anthropic-version",
                "openai-beta",
                "originator",
                "version",
            ] {
                let entries: Vec<_> = headers
                    .get_all(name)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 1024)
                    .collect();
                if !entries.is_empty() {
                    values.insert(name, json!(entries));
                }
            }
            if let Some(ua) = ua_product(headers) {
                values.insert("user-agent", json!([ua]));
            }
            // DOM이나 본문 사본 없이 관찰용 스트림에서 키만 수집한다.
            let result = decode(headers, body);
            Some(json!({ "destination": self.destination, "shape": {
                "header_names": names, "header_values": values, "body_keys": result.keys,
                "body_keys_truncated": result.truncated,
                "body_keys_available": result.available,
                "body_keys_max_depth": 3, "body_decode_limit_bytes": MAX_BODY
            } }))
        }
    }

    #[derive(Default)]
    struct KeyResult {
        keys: BTreeSet<String>,
        truncated: bool,
        available: bool,
    }

    fn decode(headers: &HeaderMap, body: &[u8]) -> KeyResult {
        let encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity");
        if encoding.eq_ignore_ascii_case("identity") {
            let mut result = KeyResult::default();
            let mut parser =
                serde_json::Deserializer::from_slice(&body[..body.len().min(MAX_BODY)]);
            let parsed = KeysSeed {
                result: &mut result,
                depth: 0,
            }
            .deserialize(&mut parser)
            .is_ok();
            result.available = parsed && parser.end().is_ok();
            result.truncated |= body.len() > MAX_BODY;
            return result;
        }
        if !encoding.eq_ignore_ascii_case("zstd") {
            return KeyResult::default();
        }
        let Ok(mut decoder) = zstd::stream::read::Decoder::new(body) else {
            return KeyResult::default();
        };
        if decoder.window_log_max(23).is_err() {
            return KeyResult::default();
        }
        let mut bounded = decoder.take(MAX_BODY as u64);
        let mut result = KeyResult::default();
        let mut buffer = BufReader::new(&mut bounded);
        let mut parser = serde_json::Deserializer::from_reader(&mut buffer);
        let parsed = KeysSeed {
            result: &mut result,
            depth: 0,
        }
        .deserialize(&mut parser)
        .is_ok();
        result.available = parsed && parser.end().is_ok();
        // 제한에 도달했을 때 한 바이트만 추가 확인하여 정확히 제한 크기도 허용한다.
        drop(buffer);
        if bounded.limit() == 0 {
            let mut probe = [0];
            match bounded.into_inner().read(&mut probe) {
                Ok(0) => {}
                Ok(_) => {
                    result.truncated = true;
                    result.available = false;
                }
                Err(_) => result.available = false,
            }
        }
        result
    }

    struct KeysSeed<'a> {
        result: &'a mut KeyResult,
        depth: usize,
    }
    impl<'de> DeserializeSeed<'de> for KeysSeed<'_> {
        type Value = ();
        fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            if self.depth >= 3 || self.result.keys.len() >= MAX_KEYS {
                if self.result.keys.len() >= MAX_KEYS {
                    self.result.truncated = true;
                }
                return IgnoredAny::deserialize(deserializer).map(|_| ());
            }
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for KeysSeed<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON shape")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while map
                .next_key_seed(KeySeed {
                    result: self.result,
                })?
                .is_some()
            {
                map.next_value_seed(KeysSeed {
                    result: self.result,
                    depth: self.depth + 1,
                })?;
            }
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            // 배열은 키 계층을 추가하지 않는다.
            while seq
                .next_element_seed(KeysSeed {
                    result: self.result,
                    depth: self.depth,
                })?
                .is_some()
            {}
            Ok(())
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: de::Error>(self) -> Result<(), E> {
            Ok(())
        }
    }
    struct KeySeed<'a> {
        result: &'a mut KeyResult,
    }
    impl<'de> DeserializeSeed<'de> for KeySeed<'_> {
        type Value = ();
        fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_str(self)
        }
    }
    impl<'de> Visitor<'de> for KeySeed<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON key")
        }
        fn visit_str<E: de::Error>(self, key: &str) -> Result<(), E> {
            if key.len() > 128 {
                self.result.truncated = true;
            } else if self.result.keys.len() < MAX_KEYS {
                self.result.keys.insert(key.to_owned());
            } else {
                self.result.truncated = true;
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        fn directory() -> tempfile::TempDir {
            let root =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/capture-tests");
            fs::create_dir_all(&root).unwrap();
            tempfile::tempdir_in(root).unwrap()
        }

        #[test]
        fn directory_must_exist_and_canonical_path_must_contain_sandbox() {
            let dir = directory();
            assert!(
                Capture::new(CaptureOptions {
                    dir: dir.path().join("missing"),
                    limit: 50
                })
                .is_err()
            );
            assert!(
                Capture::new(CaptureOptions {
                    dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                    limit: 50
                })
                .is_err()
            );
            assert!(
                Capture::new(CaptureOptions {
                    dir: dir.path().into(),
                    limit: 0
                })
                .is_err()
            );
            let fake_component =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/..");
            assert!(
                Capture::new(CaptureOptions {
                    dir: fake_component,
                    limit: 50
                })
                .is_err()
            );
            assert!(
                Capture::new(CaptureOptions {
                    dir: dir.path().into(),
                    limit: 50
                })
                .is_ok()
            );
        }

        #[test]
        fn side_decode_bounds_plain_zstd_and_invalid_encodings() {
            let mut h = HeaderMap::new();
            let mut body = br#"{"one":{"two":{"text":""#.to_vec();
            body.resize(MAX_BODY + 1, b'a');
            body.extend_from_slice(br#""}}}"#);
            let result = decode(&h, &body);
            assert!(result.truncated);
            assert!(!result.available);
            assert!(result.keys.contains("text"));
            let compressed = zstd::stream::encode_all(body.as_slice(), 1).unwrap();
            h.insert("content-encoding", "zstd".parse().unwrap());
            let result = decode(&h, &compressed);
            assert!(result.truncated);
            assert!(!result.available);
            assert!(result.keys.contains("text"));
            let result = decode(&h, b"not a zstd frame");
            assert!(!result.available);
            assert!(!result.truncated);
            h.insert("content-encoding", "gzip".parse().unwrap());
            assert!(!decode(&h, b"{}").available);
            h.insert("content-encoding", "identity".parse().unwrap());
            assert!(decode(&h, b"{}").available);
        }

        #[test]
        fn depth_and_key_count_are_bounded() {
            let result = decode(
                &HeaderMap::new(),
                br#"{"first":[{"second":{"third":{"fourth":"private"}}}]}"#,
            );
            let keys = result.keys;
            assert_eq!(keys, ["first", "second", "third"].map(str::to_owned).into());
            let object: serde_json::Map<String, Value> = (0..MAX_KEYS + 10)
                .map(|i| (format!("key{i}"), Value::Null))
                .collect();
            let body = serde_json::to_vec(&object).unwrap();
            let result = decode(&HeaderMap::new(), &body);
            assert_eq!(result.keys.len(), MAX_KEYS);
            assert!(result.available);
            assert!(result.truncated);
            let body = format!("{{\"{}\":null,\"escaped\\u005fkey\":42}}", "x".repeat(129));
            let result = decode(&HeaderMap::new(), body.as_bytes());
            assert_eq!(result.keys, ["escaped_key".to_owned()].into());
            assert!(result.available);
            assert!(result.truncated);
        }

        #[test]
        fn realistic_33mib_raw_and_zstd_capture_nested_metadata_keys_without_values() {
            let mut body = br#"{"input":[{"content":[{"text":""#.to_vec();
            body.resize(33 * 1024 * 1024, b'a');
            body.extend_from_slice(br#"fake-body-marker"}],"diagnostics":{"session_id":"fake-session-marker","installation_id":"fake-install-marker","turn_metadata":{"private_fourth":"fake-body-marker"}}}],"scalar":true,"number":1.5,"null":null}"#);
            let mut headers = HeaderMap::new();
            let raw = decode(&headers, &body);
            assert!(raw.available);
            assert!(!raw.truncated);
            for key in [
                "text",
                "diagnostics",
                "session_id",
                "installation_id",
                "turn_metadata",
            ] {
                assert!(raw.keys.contains(key), "{key}");
            }
            assert!(!raw.keys.contains("private_fourth"));
            let compressed = zstd::stream::encode_all(body.as_slice(), 1).unwrap();
            headers.insert("content-encoding", "zstd".parse().unwrap());
            let encoded = decode(&headers, &compressed);
            assert!(encoded.available);
            assert!(!encoded.truncated);
            assert_eq!(raw.keys, encoded.keys);
            let serialized = serde_json::to_string(&encoded.keys).unwrap();
            for marker in [
                "fake-body-marker",
                "fake-session-marker",
                "fake-install-marker",
            ] {
                assert!(!serialized.contains(marker));
            }
            assert!(body.ends_with(br#""null":null}"#));
        }

        #[test]
        fn oversized_body_reports_skipped_keys_without_header_values_leaking() {
            let dir = directory();
            let capture = Capture::new(CaptureOptions {
                dir: dir.path().into(),
                limit: 1,
            })
            .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                "Bearer fake-access-marker".parse().unwrap(),
            );
            let shape = capture.shape(&headers, &vec![b'a'; MAX_BODY + 1]).unwrap();
            assert_eq!(shape["shape"]["body_keys_truncated"], true);
            assert_eq!(shape["shape"]["body_keys_available"], false);
            assert_eq!(shape["shape"]["body_decode_limit_bytes"], MAX_BODY);
            assert_eq!(shape["shape"]["body_keys"], json!([]));
            assert_eq!(shape["shape"]["header_names"], json!(["authorization"]));
            assert_eq!(shape["shape"]["header_values"], json!({}));
            assert!(!shape.to_string().contains("fake-access-marker"));
            assert!(capture.shape(&headers, b"{}").is_none());
        }
    }
}

#[cfg(all(feature = "shape-capture", debug_assertions))]
pub(crate) use enabled::{Capture, response_shape};
