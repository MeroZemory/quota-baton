//! Presentation of credential origin and refresh ownership, not refresh policy.
use super::{ImportMode, Source};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshSource {
    Oauth,
    LinkedCpa,
    AdoptedCpa,
    Takeover,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshOwner {
    Cpa,
    QuotaBaton,
    Unknown,
}

impl RefreshSource {
    /// Native tagged source kind/mode, or an existing flattened display key.
    /// CPA without a recognized mode never implies either refresh owner.
    /// OAuth and takeover ownership is fixed by kind; additive modes do not change it.
    pub fn from_parts(kind: &str, mode: Option<&str>) -> Self {
        match (kind, mode) {
            ("oauth", _) | ("owned", None) => Self::Oauth,
            ("cpa", Some("linked")) | ("linked", None) => Self::LinkedCpa,
            ("cpa", Some("adopted")) | ("adopted", None) => Self::AdoptedCpa,
            ("takeover", _) => Self::Takeover,
            _ => Self::Unknown,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::LinkedCpa => "linked",
            Self::AdoptedCpa => "adopted",
            Self::Takeover => "takeover",
            Self::Unknown => "unknown",
        }
    }

    pub fn refresh_owner(self) -> RefreshOwner {
        match self {
            Self::LinkedCpa => RefreshOwner::Cpa,
            Self::Oauth | Self::AdoptedCpa | Self::Takeover => RefreshOwner::QuotaBaton,
            Self::Unknown => RefreshOwner::Unknown,
        }
    }

    pub fn origin_label(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::Oauth, false) => "직접 로그인",
            (Self::Oauth, true) => "Direct sign-in",
            (Self::LinkedCpa, false) => "CPA 연결",
            (Self::LinkedCpa, true) => "Linked to CPA",
            (Self::AdoptedCpa, false) => "CPA에서 인수",
            (Self::AdoptedCpa, true) => "Adopted from CPA",
            (Self::Takeover, false) => "다른 앱에서 인수",
            (Self::Takeover, true) => "Taken over from another app",
            (Self::Unknown, false) => "알 수 없음",
            (Self::Unknown, true) => "Unknown",
        }
    }
}

impl From<&Source> for RefreshSource {
    fn from(source: &Source) -> Self {
        match source {
            Source::Oauth => Self::Oauth,
            Source::Takeover { .. } => Self::Takeover,
            Source::Cpa { mode, .. } => match mode {
                ImportMode::Linked => Self::LinkedCpa,
                ImportMode::Adopted => Self::AdoptedCpa,
            },
        }
    }
}

impl RefreshOwner {
    pub fn label(self, english: bool) -> &'static str {
        match self {
            Self::Cpa => "CPA",
            Self::QuotaBaton => "qb",
            Self::Unknown if english => "Unknown",
            Self::Unknown => "알 수 없음",
        }
    }

    pub fn refresh_label(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::Cpa, false) => "CPA 갱신",
            (Self::Cpa, true) => "CPA refresh",
            (Self::QuotaBaton, false) => "직접 갱신",
            (Self::QuotaBaton, true) => "Direct refresh",
            (Self::Unknown, false) => "갱신 방식 확인 불가",
            (Self::Unknown, true) => "Unknown refresher",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_and_mode_match_actual_native_refresh_owner_and_shared_labels() {
        for (source, key, owner, ko, en) in [
            (
                Source::Oauth,
                "oauth",
                RefreshOwner::QuotaBaton,
                "직접 로그인",
                "Direct sign-in",
            ),
            (
                Source::Takeover {
                    from: "synthetic-home".into(),
                    at: chrono::DateTime::UNIX_EPOCH,
                },
                "takeover",
                RefreshOwner::QuotaBaton,
                "다른 앱에서 인수",
                "Taken over from another app",
            ),
            (
                cpa(ImportMode::Linked),
                "linked",
                RefreshOwner::Cpa,
                "CPA 연결",
                "Linked to CPA",
            ),
            (
                cpa(ImportMode::Adopted),
                "adopted",
                RefreshOwner::QuotaBaton,
                "CPA에서 인수",
                "Adopted from CPA",
            ),
        ] {
            let display = RefreshSource::from(&source);
            let wire = serde_json::to_value(&source).unwrap();
            assert_eq!(
                RefreshSource::from_parts(wire["kind"].as_str().unwrap(), wire["mode"].as_str()),
                display
            );
            assert_eq!(RefreshSource::from_parts(key, None), display);
            assert_eq!(display.key(), key);
            assert_eq!(display.refresh_owner(), owner);
            assert_eq!(display.origin_label(false), ko);
            assert_eq!(display.origin_label(true), en);
            let mut account = super::super::health::tests::fixture();
            account.source = source;
            assert_eq!(
                crate::relay::credentials::is_linked(&account),
                owner == RefreshOwner::Cpa
            );
        }
        for (owner, label, ko, en) in [
            (RefreshOwner::Cpa, "CPA", "CPA 갱신", "CPA refresh"),
            (
                RefreshOwner::QuotaBaton,
                "qb",
                "직접 갱신",
                "Direct refresh",
            ),
        ] {
            assert_eq!(owner.label(false), label);
            assert_eq!(owner.label(true), label);
            assert_eq!(owner.refresh_label(false), ko);
            assert_eq!(owner.refresh_label(true), en);
        }
    }

    #[test]
    fn legacy_owned_display_inputs_keep_qb_as_refresher() {
        let source = RefreshSource::from_parts("owned", None);
        assert_eq!(source, RefreshSource::Oauth);
        assert_eq!(source.key(), "oauth");
        assert_eq!(source.refresh_owner(), RefreshOwner::QuotaBaton);
        assert_eq!(RefreshSource::from_parts("oauth", Some("owned")), source);
        for mode in ["linked", "adopted", "future"] {
            assert_eq!(
                RefreshSource::from_parts("owned", Some(mode)),
                RefreshSource::Unknown
            );
        }
    }

    #[test]
    fn oauth_and_takeover_keep_qb_ownership_with_additive_modes() {
        for (kind, expected) in [
            ("oauth", RefreshSource::Oauth),
            ("takeover", RefreshSource::Takeover),
        ] {
            for mode in [
                None,
                Some("owned"),
                Some("linked"),
                Some("adopted"),
                Some("future"),
                Some(""),
            ] {
                let source = RefreshSource::from_parts(kind, mode);
                assert_eq!(source, expected);
                assert_eq!(source.refresh_owner(), RefreshOwner::QuotaBaton);
                assert_eq!(source.refresh_owner().label(false), "qb");
                assert_eq!(source.refresh_owner().label(true), "qb");
            }
        }
    }

    #[test]
    fn ambiguous_or_future_source_never_claims_a_refresher() {
        for (kind, mode) in [
            ("cpa", None),
            ("cpa", Some("future")),
            ("future", None),
            ("", None),
        ] {
            let source = RefreshSource::from_parts(kind, mode);
            assert_eq!(source, RefreshSource::Unknown);
            assert_eq!(source.refresh_owner(), RefreshOwner::Unknown);
            assert_eq!(source.refresh_owner().label(false), "알 수 없음");
            assert_eq!(source.refresh_owner().label(true), "Unknown");
            assert_eq!(
                source.refresh_owner().refresh_label(false),
                "갱신 방식 확인 불가"
            );
            assert_eq!(
                source.refresh_owner().refresh_label(true),
                "Unknown refresher"
            );
        }
    }

    fn cpa(mode: ImportMode) -> Source {
        Source::Cpa {
            mode,
            dir: "synthetic-cpa".into(),
            r#match: super::super::SourceMatch::Codex {
                account_id: "synthetic-account".into(),
                email: "synthetic@example.invalid".into(),
            },
        }
    }
}
