//! Authenticated box hints are preferences, never session or account authority.
use axum::http::HeaderMap;

use super::BoxAuthority;
use crate::accounts::Provider;

pub(crate) const ROOT_HEADER: &str = "x-qb-root-session";
const MAX_ROOT_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RootHint(String);

impl RootHint {
    /// Parsing supplies no permission; host-admin registration can reuse this.
    pub fn parse(value: &str) -> Result<Self, LineageError> {
        valid_root(value)
            .then(|| Self(value.to_owned()))
            .ok_or(LineageError::Malformed)
    }

    pub fn for_provider(&self, provider: Provider) -> Option<&str> {
        let prefix = match provider {
            Provider::Claude => "claude:",
            Provider::Codex => "codex:",
        };
        self.0.starts_with(prefix).then_some(self.0.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineageError {
    Malformed,
    Revoked,
}

/// Always remove every occurrence, including on ordinary TCP and on rejection.
/// The authority must be the middleware's current authenticated registry grant.
pub(crate) fn take_root(
    headers: &mut HeaderMap,
    authority: Option<&BoxAuthority>,
) -> Result<Option<RootHint>, LineageError> {
    let parsed = if authority.is_some() {
        let mut values = headers.get_all(ROOT_HEADER).iter();
        match (values.next(), values.next()) {
            (None, _) => Ok(None),
            (Some(value), None) => value
                .to_str()
                .map_err(|_| LineageError::Malformed)
                .and_then(RootHint::parse)
                .map(Some),
            _ => Err(LineageError::Malformed),
        }
    } else {
        Ok(None)
    };
    headers.remove(ROOT_HEADER);
    if authority.is_some_and(BoxAuthority::is_revoked) {
        return Err(LineageError::Revoked);
    }
    parsed
}

fn valid_root(value: &str) -> bool {
    if value.len() > MAX_ROOT_BYTES {
        return false;
    }
    let Some(id) = value
        .strip_prefix("codex:")
        .or_else(|| value.strip_prefix("claude:"))
    else {
        return false;
    };
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{BoxName, BoxRegistry};
    use axum::http::HeaderValue;

    fn fixture() -> (tempfile::TempDir, BoxRegistry, BoxAuthority) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-lineage");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("sample-box").unwrap())
            .unwrap()
            .into_parts();
        let authority = registry.authenticate(&scope, &key).unwrap();
        (home, registry, authority)
    }

    #[test]
    fn tcp_discards_all_hints_without_promoting_them_to_authority() {
        let mut headers = HeaderMap::new();
        headers.append(ROOT_HEADER, HeaderValue::from_static("codex:sample-root"));
        headers.append(ROOT_HEADER, HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(take_root(&mut headers, None), Ok(None));
        assert!(!headers.contains_key(ROOT_HEADER));
    }

    #[test]
    fn authenticated_hint_preserves_exact_root_and_provider_boundary() {
        let (_home, _registry, authority) = fixture();
        for (value, provider, other) in [
            ("codex:sample-root", Provider::Codex, Provider::Claude),
            ("claude:sample_2", Provider::Claude, Provider::Codex),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(ROOT_HEADER, HeaderValue::from_str(value).unwrap());
            let hint = take_root(&mut headers, Some(&authority)).unwrap().unwrap();
            assert_eq!(hint.for_provider(provider), Some(value));
            assert_eq!(hint.for_provider(other), None);
            assert!(!headers.contains_key(ROOT_HEADER));
        }
    }

    #[test]
    fn malformed_duplicate_and_revoked_hints_are_removed_before_refusal() {
        let (_home, registry, authority) = fixture();
        let oversized = format!("codex:{}", "a".repeat(MAX_ROOT_BYTES));
        for value in [
            "",
            "codex:",
            "sample-root",
            "codex:sample/root",
            "claude:sample:agent:child",
            " codex:sample",
            "codex:sample,other",
            oversized.as_str(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(ROOT_HEADER, HeaderValue::from_str(value).unwrap());
            assert_eq!(
                take_root(&mut headers, Some(&authority)),
                Err(LineageError::Malformed)
            );
            assert!(!headers.contains_key(ROOT_HEADER));
        }
        let mut headers = HeaderMap::new();
        headers.append(ROOT_HEADER, HeaderValue::from_static("codex:sample"));
        headers.append(ROOT_HEADER, HeaderValue::from_static("codex:sample"));
        assert_eq!(
            take_root(&mut headers, Some(&authority)),
            Err(LineageError::Malformed)
        );
        assert!(!headers.contains_key(ROOT_HEADER));
        headers.insert(ROOT_HEADER, HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(
            take_root(&mut headers, Some(&authority)),
            Err(LineageError::Malformed)
        );
        assert!(!headers.contains_key(ROOT_HEADER));
        registry.revoke(authority.scope().name()).unwrap();
        headers.insert(ROOT_HEADER, HeaderValue::from_static("codex:sample"));
        assert_eq!(
            take_root(&mut headers, Some(&authority)),
            Err(LineageError::Revoked)
        );
        assert!(!headers.contains_key(ROOT_HEADER));
    }
}
