//! Launch-path normalization without authentication or binding resolution.
use axum::http::Uri;
use zeroize::Zeroizing;

pub(crate) struct LaunchPath {
    pub(crate) token: Option<Zeroizing<String>>,
    pub(crate) uri: Uri,
}

impl std::fmt::Debug for LaunchPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchPath")
            .field("has_token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

/// Never return a path/token-bearing error. Authentication remains outside this
/// helper. A syntactically present but invalid token is resolved later, so an
/// existing bound session can still win as D22 requires.
pub(crate) fn normalize(uri: &Uri) -> Result<LaunchPath, ()> {
    let Some(rest) = uri.path().strip_prefix("/l/") else {
        return Ok(LaunchPath {
            token: None,
            uri: uri.clone(),
        });
    };
    let (token, suffix) = rest.split_once('/').ok_or(())?;
    if !suffix.starts_with("v1/") {
        return Err(());
    }
    // Token registry accepts only 43 URL-safe bytes. Keep malformed/oversized
    // handles as an unresolvable marker, never duplicate unbounded path bytes.
    let token = if token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        token
    } else {
        ""
    };
    let mut path = format!("/{suffix}");
    if let Some(query) = uri.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path.parse().map_err(|_| ())?);
    Ok(LaunchPath {
        token: Some(Zeroizing::new(token.to_owned())),
        uri: Uri::from_parts(parts).map_err(|_| ())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn strips_only_launch_prefix_preserving_route_and_raw_query() {
        let uri: Uri = format!("/l/{TOKEN}/v1/responses?a=%2F&a=2")
            .parse()
            .unwrap();
        let path = normalize(&uri).unwrap();
        assert_eq!(path.uri.to_string(), "/v1/responses?a=%2F&a=2");
        assert_eq!(path.token.as_deref().map(String::as_str), Some(TOKEN));
        assert!(!format!("{path:?}").contains(TOKEN));
        assert!(!path.uri.to_string().contains(TOKEN));
    }

    #[test]
    fn keeps_unprefixed_routes_unchanged_and_does_not_intercept_similar_paths() {
        for raw in [
            "/v1/responses",
            "/v1/models?client_version=0.163.0",
            "/long/test",
        ] {
            let uri: Uri = raw.parse().unwrap();
            let path = normalize(&uri).unwrap();
            assert_eq!(path.uri, uri);
            assert!(path.token.is_none());
        }
    }

    #[test]
    fn malformed_handles_keep_route_but_never_become_authentication() {
        for token in ["", "unknown", "abc%2Fdef", &"x".repeat(4096)] {
            let uri: Uri = format!("/l/{token}/v1/responses").parse().unwrap();
            let path = normalize(&uri).unwrap();
            assert_eq!(path.uri.path(), "/v1/responses");
            assert_eq!(path.token.as_deref().map(String::as_str), Some(""));
        }
    }

    #[test]
    fn rejects_non_data_prefixes_without_echoing_raw_paths() {
        for suffix in ["admin/v1/state", "", "v1", "l/x/v1/responses"] {
            let uri: Uri = format!("/l/{TOKEN}/{suffix}").parse().unwrap();
            assert!(normalize(&uri).is_err());
        }
    }
}
