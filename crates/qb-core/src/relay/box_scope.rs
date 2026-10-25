//! Proposed relay child; authority is obtained only from protected ingress.
use crate::boxes::BoxAuthority;
use crate::launch_tokens::{LaunchPin, LaunchTokenError, LaunchTokenRegistry};
use std::sync::Arc;

/// Scope errors are hard refusals before any C8 existing-session fallback.
/// Ordinary TCP keeps its historical missing/expired fallback behavior.
pub(super) fn pin_token(
    registry: &LaunchTokenRegistry,
    token: &str,
    authority: Option<&BoxAuthority>,
) -> Result<Option<Arc<LaunchPin>>, ()> {
    if authority.is_some_and(BoxAuthority::is_revoked) {
        return Err(());
    }
    match registry.get_or_pin_scoped(token, authority.map(BoxAuthority::scope)) {
        Ok(pin) => Ok(Some(Arc::new(pin))),
        Err(LaunchTokenError::ScopeMismatch) => Err(()),
        Err(_) if authority.is_some() => Err(()),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{BoxName, BoxRegistry};
    use crate::launch_tokens::LaunchMetadata;
    use std::collections::BTreeSet;

    #[test]
    fn exact_socket_scope_is_required_before_legacy_fallback() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-relay-scope");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let boxes = BoxRegistry::open(home.path()).unwrap();
        let (a, a_key) = boxes
            .add(BoxName::parse("sample-a").unwrap())
            .unwrap()
            .into_parts();
        let (b, b_key) = boxes
            .add(BoxName::parse("sample-b").unwrap())
            .unwrap()
            .into_parts();
        let a_auth = boxes.authenticate(&a, &a_key).unwrap();
        let b_auth = boxes.authenticate(&b, &b_key).unwrap();
        let tokens = LaunchTokenRegistry::default();
        let metadata = || LaunchMetadata {
            binding_anchor_key: "launch:sample".to_owned(),
            advertised_models: BTreeSet::new(),
        };
        let scoped = tokens.issue_scoped(metadata(), a.clone()).unwrap();
        let ordinary = tokens.issue(metadata()).unwrap();
        assert!(
            pin_token(&tokens, scoped.expose_secret(), Some(&a_auth))
                .unwrap()
                .is_some()
        );
        assert!(pin_token(&tokens, scoped.expose_secret(), Some(&b_auth)).is_err());
        assert!(pin_token(&tokens, scoped.expose_secret(), None).is_err());
        assert!(pin_token(&tokens, ordinary.expose_secret(), Some(&a_auth)).is_err());
        assert!(
            pin_token(&tokens, ordinary.expose_secret(), None)
                .unwrap()
                .is_some()
        );
        assert!(pin_token(&tokens, "unknown", None).unwrap().is_none());
        assert!(pin_token(&tokens, "unknown", Some(&a_auth)).is_err());
        boxes.revoke(a.name()).unwrap();
        assert!(pin_token(&tokens, scoped.expose_secret(), Some(&a_auth)).is_err());
        // The old epoch token stays unusable on a newly enrolled same-name socket.
        let (new_a, key) = boxes.add(a.name().clone()).unwrap().into_parts();
        let new_auth = boxes.authenticate(&new_a, &key).unwrap();
        assert!(pin_token(&tokens, scoped.expose_secret(), Some(&new_auth)).is_err());
    }
}
