use qb_core::accounts::refresh_owner::{RefreshOwner, RefreshSource};
use qb_tray::{client::PresentationAccount, i18n, v2};
use serde_json::json;
use std::time::SystemTime;

#[test]
fn r19_source_table_keeps_native_cli_labels_and_both_tray_adapters_consistent() {
    for (source, owner, origin_ko, origin_en) in [
        (
            json!({"kind":"oauth"}),
            RefreshOwner::QuotaBaton,
            "직접 로그인",
            "Direct sign-in",
        ),
        (
            json!({"kind":"cpa","mode":"linked"}),
            RefreshOwner::Cpa,
            "CPA 연결",
            "Linked to CPA",
        ),
        (
            json!({"kind":"cpa","mode":"adopted"}),
            RefreshOwner::QuotaBaton,
            "CPA에서 인수",
            "Adopted from CPA",
        ),
        (
            json!({"kind":"takeover","from":"synthetic-home","at":"2026-01-01T00:00:00Z"}),
            RefreshOwner::QuotaBaton,
            "다른 앱에서 인수",
            "Taken over from another app",
        ),
        (
            json!({"kind":"cpa","mode":"future"}),
            RefreshOwner::Unknown,
            "알 수 없음",
            "Unknown",
        ),
        (
            json!({"kind":"oauth","mode":"linked"}),
            RefreshOwner::QuotaBaton,
            "직접 로그인",
            "Direct sign-in",
        ),
        (
            json!({"kind":"future","mode":"adopted"}),
            RefreshOwner::Unknown,
            "알 수 없음",
            "Unknown",
        ),
    ] {
        let refresh =
            RefreshSource::from_parts(source["kind"].as_str().unwrap(), source["mode"].as_str());
        assert_eq!(refresh.refresh_owner(), owner);
        let account = json!({"id":"sample-acct","provider":"codex","plan":"plus","refresh_attention":true,
            "user_state":"active","health":{"kind":"ready"},"source":source});
        for (language, english, origin, menu_identity) in [
            (
                i18n::UiLanguage::Ko,
                false,
                origin_ko,
                match owner {
                    RefreshOwner::Cpa => "CPA가 토큰 갱신",
                    RefreshOwner::QuotaBaton => "토큰 직접 갱신",
                    RefreshOwner::Unknown => "토큰 갱신 방식 확인 전",
                },
            ),
            (
                i18n::UiLanguage::En,
                true,
                origin_en,
                match owner {
                    RefreshOwner::Cpa => "CPA refreshes tokens",
                    RefreshOwner::QuotaBaton => "Direct token refresh",
                    RefreshOwner::Unknown => "Token refresh method not yet checked",
                },
            ),
        ] {
            i18n::with_language(language, || {
                assert_eq!(refresh.origin_label(english), origin);
                let input = json!({"accounts":[account.clone()]}).to_string();
                let rows = qb_tray::parse_state_at(&input, SystemTime::UNIX_EPOCH).unwrap();
                assert_eq!(
                    qb_tray::account_menu_metadata(&rows[0])[0],
                    owner.refresh_label(english)
                );
                let adapted: PresentationAccount = serde_json::from_value(account.clone()).unwrap();
                let adapted_rows = qb_tray::parse_state_at(
                    &json!({"accounts":[adapted]}).to_string(),
                    SystemTime::UNIX_EPOCH,
                )
                .unwrap();
                assert_eq!(
                    qb_tray::account_menu_metadata(&adapted_rows[0])[0],
                    owner.refresh_label(english)
                );
                let view = v2::project(&input, SystemTime::UNIX_EPOCH).unwrap();
                let row = view
                    .sections
                    .iter()
                    .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
                    .next()
                    .unwrap();
                assert!(
                    row.menu.identity_line.contains(menu_identity),
                    "{}",
                    row.menu.identity_line
                );
                assert_eq!(
                    row.menu.show_refresh_token,
                    owner == RefreshOwner::QuotaBaton
                );
            });
        }
    }
}
