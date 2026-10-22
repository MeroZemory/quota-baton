//! Private display evidence. No filesystem, environment, transcript or network reads.
use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{
    Deserialize, Serialize,
    de::{IgnoredAny, MapAccess, Visitor},
};

pub const LABEL_HEADER: &str = "x-quota-baton-label";
const LABEL_BYTES: usize = 256;
const WIRE_BYTES: usize = 346;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    Launcher,
    CodexWorkspace,
    ClaudeProject,
}

impl LabelSource {
    fn rank(self) -> u8 {
        match self {
            Self::Launcher => 3,
            Self::CodexWorkspace => 2,
            Self::ClaudeProject => 1,
        }
    }
}

// Deliberately neither Serialize/Deserialize nor Debug. Disk types cannot hold this.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryLabel {
    text: String,
    source: LabelSource,
}

#[derive(Clone, Serialize)]
pub struct LabelView {
    pub text: String,
    pub source: LabelSource,
    /// Filename slug reconstruction is advisory, including unique local matches.
    pub best_effort: bool,
}

impl MemoryLabel {
    pub fn retained_bytes(&self) -> usize {
        self.text.capacity().saturating_add(64)
    }
    pub fn new(text: &str, source: LabelSource) -> Option<Self> {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        let basename = cleaned
            .trim()
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()?
            .trim();
        if basename.is_empty() || matches!(basename, "." | "..") || basename.contains(':') {
            return None;
        }
        let text = basename.chars().take(64).collect::<String>();
        Some(Self { text, source })
    }
    pub fn view(&self) -> LabelView {
        LabelView {
            text: self.text.clone(),
            source: self.source,
            best_effort: self.source == LabelSource::ClaudeProject,
        }
    }
    pub fn replace_if_better(current: &mut Option<Self>, incoming: Option<Self>) -> bool {
        let Some(next) = incoming else {
            return false;
        };
        if current
            .as_ref()
            .is_some_and(|old| old.source.rank() > next.source.rank() || old == &next)
        {
            return false;
        }
        *current = Some(next);
        true
    }
}

// Distinct from folder labels and never serialized as part of a general record.
#[derive(Clone)]
pub struct OriginalShortId(String);
impl OriginalShortId {
    pub fn retained_bytes(&self) -> usize {
        self.0.capacity().saturating_add(64)
    }
    pub fn from_root_key(key: &str) -> Option<Self> {
        let root = super::group_root(key);
        let (_, raw) = root.split_once(':')?;
        if raw.is_empty()
            || !raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return None;
        }
        Some(Self(raw.chars().take(8).collect()))
    }
    pub fn detail_value(&self) -> String {
        self.0.clone()
    }
}

/// Must run on both HTTP and WS entry BEFORE capture/upstream construction.
pub fn take_launcher_label(headers: &mut HeaderMap) -> Option<MemoryLabel> {
    let value = {
        let mut values = headers.get_all(LABEL_HEADER).iter();
        let first = values.next().and_then(|v| v.to_str().ok());
        if values.next().is_some() {
            None
        } else {
            first.and_then(parse_launcher_value)
        }
    };
    headers.remove(LABEL_HEADER); // All instances, including duplicate/invalid input.
    value
}

/// qb1 is decoded exactly once. Plain ASCII remains a bounded compatibility path.
pub fn parse_launcher_value(value: &str) -> Option<MemoryLabel> {
    if value.len() > WIRE_BYTES || !value.is_ascii() {
        return None;
    }
    let decoded;
    let text = if let Some(payload) = value.strip_prefix("qb1:") {
        if payload.is_empty()
            || !payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
        if bytes.len() > LABEL_BYTES {
            return None;
        }
        decoded = String::from_utf8(bytes).ok()?;
        decoded.as_str()
    } else {
        value
    };
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() > 64 {
        return None;
    }
    MemoryLabel::new(&cleaned, LabelSource::Launcher)
}

/// Capture must exclude the header NAME too, even if it sees original headers.
pub fn capture_header_allowed(name: &str) -> bool {
    !name.eq_ignore_ascii_case(LABEL_HEADER)
}

#[derive(Default)]
struct FirstWorkspace(Option<String>);
impl<'de> Deserialize<'de> for FirstWorkspace {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct First;
        impl<'de> Visitor<'de> for First {
            type Value = FirstWorkspace;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a workspace map")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut first = None;
                let mut count = 0;
                while let Some(key) = map.next_key::<String>()? {
                    count += 1;
                    if count > 128 {
                        return Err(serde::de::Error::custom("workspace limit"));
                    }
                    if first.is_none() {
                        first = Some(key);
                    }
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(FirstWorkspace(first))
            }
        }
        de.deserialize_map(First)
    }
}

pub fn codex_workspace(headers: &HeaderMap) -> Option<MemoryLabel> {
    let raw = crate::session::json_turn_metadata(headers)?;
    #[derive(Deserialize)]
    struct Turn {
        #[serde(default)]
        workspaces: FirstWorkspace,
    }
    let turn: Turn = serde_json::from_str(raw).ok()?;
    MemoryLabel::new(turn.workspaces.0.as_deref()?, LabelSource::CodexWorkspace)
}

/// A production scanner supplies filenames only. This reducer never opens a file.
/// The callback returns a basename or the suffix after verified parent names;
/// without that evidence, preserve the whole slug rather than splitting it.
pub fn claude_project_label<'a>(
    sid: &str,
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    mut decoded_existing_basename: impl FnMut(&str) -> Option<String>,
) -> Option<MemoryLabel> {
    if sid.is_empty()
        || sid.len() > 128
        || !sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    let filename = format!("{sid}.jsonl");
    for (slug, entry) in entries.into_iter().take(4096) {
        if entry != filename {
            continue;
        }
        let decoded = decoded_existing_basename(slug);
        let text = decoded.as_deref().unwrap_or(slug);
        return MemoryLabel::new(text, LabelSource::ClaudeProject);
    }
    None
}

#[derive(Default)]
pub struct LookupBudget {
    attempts: u8,
    first_ms: Option<u64>,
}
impl LookupBudget {
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if self.attempts == 0 {
            self.attempts = 1;
            self.first_ms = Some(now_ms);
            true
        } else if self.attempts == 1
            && now_ms.saturating_sub(self.first_ms.unwrap_or(now_ms)) >= 60_000
        {
            self.attempts = 2;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn label_precedence_basename_unicode_limits_and_empty_updates() {
        let mut current = MemoryLabel::new("C:\\private\\project-one", LabelSource::CodexWorkspace);
        assert_eq!(current.as_ref().unwrap().view().text, "project-one");
        assert!(!MemoryLabel::replace_if_better(
            &mut current,
            MemoryLabel::new("other", LabelSource::ClaudeProject)
        ));
        assert!(MemoryLabel::replace_if_better(
            &mut current,
            MemoryLabel::new("/workspace/작업\n", LabelSource::Launcher)
        ));
        assert_eq!(current.as_ref().unwrap().view().text, "작업");
        assert!(!MemoryLabel::replace_if_better(&mut current, None));
        assert_eq!(
            MemoryLabel::new(&"가".repeat(100), LabelSource::Launcher)
                .unwrap()
                .view()
                .text
                .chars()
                .count(),
            64
        );
        for invalid in ["", "..", "/", "C:\\"] {
            assert!(MemoryLabel::new(invalid, LabelSource::Launcher).is_none());
        }
    }
    #[test]
    fn header_is_removed_even_when_duplicate_or_invalid() {
        let mut headers = HeaderMap::new();
        headers.append(LABEL_HEADER, "folder-one".parse().unwrap());
        headers.append(LABEL_HEADER, "folder-two".parse().unwrap());
        assert!(take_launcher_label(&mut headers).is_none());
        assert!(!headers.contains_key(LABEL_HEADER));
        headers.insert(LABEL_HEADER, "folder".parse().unwrap());
        assert_eq!(
            take_launcher_label(&mut headers).unwrap().view().text,
            "folder"
        );
        assert!(!headers.contains_key(LABEL_HEADER));
    }
    #[test]
    fn workspace_uses_wire_first_key_and_never_remote_or_commit() {
        let mut h = HeaderMap::new();
        h.insert("x-codex-turn-metadata", r#"{"workspaces":{"Z:\\private\\z-first":{"git_remote":"SECRET_REMOTE"},"A:\\a-second":{"commit":"SECRET_COMMIT"}}}"#.parse().unwrap());
        let view = codex_workspace(&h).unwrap().view();
        assert_eq!(view.text, "z-first");
        let json = serde_json::to_string(&view).unwrap();
        for secret in ["private", "SECRET_REMOTE", "SECRET_COMMIT"] {
            assert!(!json.contains(secret));
        }
    }
    #[test]
    fn unicode_workspace_json_uses_raw_utf8_header_bytes_and_invalid_utf8_is_unknown() {
        let mut headers = HeaderMap::new();
        let raw =
            r#"{"session_id":"unicode-fixture","workspaces":{"C:\\private\\한글프로젝트":{}}}"#;
        headers.insert(
            "x-codex-turn-metadata",
            axum::http::HeaderValue::from_bytes(raw.as_bytes()).unwrap(),
        );
        assert_eq!(
            codex_workspace(&headers).unwrap().view().text,
            "한글프로젝트"
        );
        headers.insert(
            "x-codex-turn-metadata",
            axum::http::HeaderValue::from_bytes(b"{\"workspaces\":{\"\xff\":{}}}").unwrap(),
        );
        assert!(codex_workspace(&headers).is_none());
        headers.insert(
            "x-codex-turn-metadata",
            axum::http::HeaderValue::from_bytes(raw.as_bytes()).unwrap(),
        );
        headers.append("x-codex-turn-metadata", "{}".parse().unwrap());
        assert!(codex_workspace(&headers).is_none());
    }
    #[test]
    fn original_prefix_and_claude_agents_are_distinct_private_evidence() {
        assert_eq!(
            OriginalShortId::from_root_key("claude:12345678-private-tail:agent:private-agent")
                .unwrap()
                .detail_value(),
            "12345678"
        );
        assert!(OriginalShortId::from_root_key("codex:../folder").is_none());
        assert_eq!(
            super::super::group_root("codex:thread:agent:unchanged"),
            "codex:thread:agent:unchanged"
        );
    }
    #[test]
    fn filename_lookup_is_exact_bounded_and_retries_once() {
        let entries = [
            ("C--private-project-one", "abcdefghi.jsonl"),
            ("C--private-project-two", "abcdefgh.jsonl"),
        ];
        let label =
            claude_project_label("abcdefgh", entries, |_| Some("project-two".into())).unwrap();
        assert_eq!(label.view().text, "project-two");
        assert!(
            claude_project_label("../abcdefgh", entries, |_| panic!(
                "must not resolve unsafe filename"
            ))
            .is_none()
        );
        let mut budget = LookupBudget::default();
        assert!(budget.admit(10));
        assert!(!budget.admit(60_009));
        assert!(budget.admit(60_010));
        assert!(!budget.admit(999_999));
    }
    #[test]
    fn qb1_utf8_ascii_compatibility_and_strict_malformed_rejection() {
        let wire = format!("qb1:{}", URL_SAFE_NO_PAD.encode("작업-폴더\n"));
        assert_eq!(
            parse_launcher_value(&wire).unwrap().view().text,
            "작업-폴더"
        );
        assert_eq!(
            parse_launcher_value("plain-ascii").unwrap().view().text,
            "plain-ascii"
        );
        for bad in ["qb1:", "qb1:Zg==", "qb1:***", "qb2:abc", "qb1:_w", "한글"] {
            assert!(parse_launcher_value(bad).is_none(), "invalid wire accepted");
        }
        assert!(
            parse_launcher_value(&format!("qb1:{}", URL_SAFE_NO_PAD.encode("가".repeat(65))))
                .is_none()
        );
        assert!(parse_launcher_value(&"a".repeat(347)).is_none());
        let decoded_path = format!(
            "qb1:{}",
            URL_SAFE_NO_PAD.encode("C:\\private-parent\\last-folder")
        );
        assert_eq!(
            parse_launcher_value(&decoded_path).unwrap().view().text,
            "last-folder"
        );
        assert!(!capture_header_allowed("X-Quota-Baton-Label"));
        assert!(capture_header_allowed("originator"));
        let mut headers = HeaderMap::new();
        headers.insert(LABEL_HEADER, "qb1:***".parse().unwrap());
        assert!(take_launcher_label(&mut headers).is_none());
        assert!(!headers.contains_key(LABEL_HEADER));
    }
}

#[derive(Clone)]
pub(crate) struct IngressLabel(pub Option<MemoryLabel>);
