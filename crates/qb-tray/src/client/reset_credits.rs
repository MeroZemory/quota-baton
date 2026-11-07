//! Reset-credit transport and pure consent state. No retries, remembered consent or profile I/O.
use super::{AdminClient, ClientError};
use chrono::{DateTime, Utc};
use qb_proto::reset_credit_action::{
    ResetCreditCommitRequest, ResetCreditOutcome, ResetCreditPreview, ResetCreditResult,
};
use qb_proto::reset_credits::{
    ResetCreditExpiry, ResetCreditInventory, ResetCreditRefusal, ResetCreditState,
    ResetCreditStatus, ResetCreditType,
};
use std::collections::BTreeMap;

fn text(ko: &'static str) -> String {
    crate::i18n::static_text(ko).into()
}

#[derive(Clone, Debug)]
pub struct InventoryLine {
    pub text: String,
    pub tooltip: String,
    pub can_use: bool,
}

pub fn expiry(value: &ResetCreditExpiry) -> String {
    match value {
        ResetCreditExpiry::At { at } => at.clone(),
        ResetCreditExpiry::Never => text("만료 없음"),
        ResetCreditExpiry::Unknown => text("만료 확인 전"),
    }
}

/// Display claims require complete current evidence, not a minimum of capped rows.
pub fn inventory_line(
    provider: &str,
    inventory: Option<&ResetCreditInventory>,
    now: DateTime<Utc>,
) -> InventoryLine {
    let mut line = InventoryLine {
        text: text("리셋권 · 확인 전"),
        tooltip: text("리셋권을 새로고침해 주세요."),
        can_use: false,
    };
    if provider != "codex" || inventory.is_some_and(|i| i.state == ResetCreditState::Unsupported) {
        line.text = text("리셋권 · 지원 안 함");
        line.tooltip = line.text.clone();
        return line;
    }
    let Some(i) = inventory else { return line };
    let observed = i
        .observed_at
        .as_deref()
        .unwrap_or(crate::i18n::static_text("확인 전"));
    let choice = qb_core::reset_credit_display_choice(i, now);
    let reported = i
        .available_count
        .map_or_else(|| "?".into(), |n| n.to_string());
    line.text = if matches!(&choice, Err(ResetCreditRefusal::Stale)) {
        crate::i18n::format("리셋권 · 오래된 정보 · 제공자 보고 {0}", &[reported])
    } else if i.state == ResetCreditState::Unknown {
        text("리셋권 · 확인 전")
    } else if matches!(&choice, Err(ResetCreditRefusal::NoCredits))
        && i.available_count == Some(0)
        && matches!(i.refusal, None | Some(ResetCreditRefusal::NoCredits))
    {
        text("리셋권 0개")
    } else if let Ok(grant) = &choice
        && i.refusal.is_none()
    {
        line.can_use = true;
        crate::i18n::format(
            "리셋권 {0}개 · 가장 빠른 만료 {1}",
            &[reported, expiry(&grant.expires_at)],
        )
    } else {
        crate::i18n::format(
            "리셋권 · 제공자 보고 {0} · 상세·만료 확인 필요",
            &[reported],
        )
    };
    let reason = i
        .refusal
        .or_else(|| choice.as_ref().err().copied())
        .map(refusal_text)
        .unwrap_or_else(|| text("만료가 가장 빠른 리셋권 1개를 사용합니다."));
    line.tooltip = format!(
        "{}\n{}\n{}",
        line.text,
        crate::i18n::format("확인 시각 {0}", &[observed.into()]),
        reason
    );
    for grant in &i.grants {
        line.tooltip.push_str(&format!(
            "\n{}",
            crate::i18n::format("기록된 만료 {0}", &[expiry(&grant.expires_at)])
        ));
    }
    line
}

fn refusal_text(r: qb_proto::reset_credits::ResetCreditRefusal) -> String {
    use qb_proto::reset_credits::ResetCreditRefusal::*;
    text(match r {
        Unknown => "리셋권을 새로고침해 주세요.",
        Unsupported => "리셋권 · 지원 안 함",
        Stale => "리셋권 정보가 변경되었거나 만료되었습니다. 새 미리보기를 열어 주세요.",
        NoCredits => "사용 가능한 리셋권이 없습니다.",
        Incomplete => "리셋권 상세 정보가 불완전하여 가장 빠른 만료를 확인할 수 없습니다.",
        UnknownEligibility => "리셋권 사용 자격을 확인하지 못했습니다.",
        UnknownExpiry => "리셋권 만료를 확인하지 못했습니다.",
        Malformed | DuplicateIds => "리셋권 정보가 올바르지 않습니다. 새로고침하세요.",
    })
}

pub const UNKNOWN: &str = "사용 결과를 확인하지 못했습니다. 상태를 다시 확인해 주세요.";
pub const STALE: &str = "리셋권 정보가 변경되었거나 만료되었습니다. 새 미리보기를 열어 주세요.";
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("reset-credit preview is stale")]
    Stale,
    #[error("reset-credit outcome is unknown")]
    Unknown,
    #[error("reset-credit read failed: {0}")]
    Read(#[source] ClientError),
}
impl ActionError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Stale => STALE,
            Self::Unknown => UNKNOWN,
            Self::Read(_) => "리셋권을 확인하지 못했습니다. 새로고침하세요.",
        }
    }
}

fn response_error(status: reqwest::StatusCode, bytes: &[u8], commit: bool) -> ActionError {
    let code = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .map(str::to_owned)
        });
    if status == reqwest::StatusCode::CONFLICT && code.as_deref() == Some("reset_credit_stale") {
        ActionError::Stale
    } else if commit || code.as_deref() == Some("reset_credit_unknown") {
        ActionError::Unknown
    } else {
        ActionError::Read(ClientError::AccountAction(
            "리셋권을 확인하지 못했습니다. 새로고침하세요.",
        ))
    }
}

impl AdminClient {
    async fn reset_request<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        action: &str,
        body: Option<serde_json::Value>,
        commit: bool,
    ) -> Result<T, ActionError> {
        // A single path segment only. Never allow an account ID to redirect a mutation.
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ActionError::Read(ClientError::AccountAction(
                "계정 번호가 올바르지 않습니다",
            )));
        }
        let path = format!("/admin/v1/accounts/{id}/reset-credits{action}");
        let request = if let Some(body) = body {
            self.request(reqwest::Method::POST, &path).json(&body)
        } else {
            self.request(reqwest::Method::GET, &path)
        };
        let mut response = request
            .timeout(std::time::Duration::from_secs(45))
            .send()
            .await
            .map_err(|e| {
                if commit {
                    ActionError::Unknown
                } else {
                    ActionError::Read(ClientError::Transport(e))
                }
            })?;
        let status = response.status();
        const LIMIT: usize = 256 * 1024;
        let mut bytes = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(|e| {
                if commit {
                    ActionError::Unknown
                } else {
                    ActionError::Read(ClientError::Transport(e))
                }
            })?;
            let Some(chunk) = chunk else { break };
            if chunk.len() > LIMIT.saturating_sub(bytes.len()) {
                return Err(if commit {
                    ActionError::Unknown
                } else {
                    ActionError::Read(ClientError::AccountAction("리셋권 응답이 너무 큽니다"))
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(response_error(status, &bytes, commit));
        }
        Self::decode_dashboard_object(&bytes).map_err(|e| {
            if commit {
                ActionError::Unknown
            } else {
                ActionError::Read(e)
            }
        })
    }
    pub async fn reset_credit_inventory(
        &self,
        id: &str,
    ) -> Result<ResetCreditInventory, ActionError> {
        self.reset_request(id, "", None, false).await
    }
    pub async fn refresh_reset_credits(
        &self,
        id: &str,
    ) -> Result<ResetCreditInventory, ActionError> {
        self.reset_request(id, "/refresh", Some(serde_json::json!({})), false)
            .await
    }
    pub async fn preview_reset_credit(&self, id: &str) -> Result<ResetCreditPreview, ActionError> {
        let p: ResetCreditPreview = self
            .reset_request(id, "/preview", Some(serde_json::json!({})), false)
            .await?;
        if p.account != id || p.effect != "eligible_codex_limits" || p.preview_token.is_empty() {
            return Err(ActionError::Stale);
        }
        Ok(p)
    }
    /// Exactly one dispatch. No generic account-action retry or authentication replay.
    pub async fn commit_reset_credit(
        &self,
        id: &str,
        request: ResetCreditCommitRequest,
    ) -> Result<ResetCreditResult, ActionError> {
        if !request.confirm {
            return Err(ActionError::Stale);
        }
        let r: ResetCreditResult = self
            .reset_request(id, "/commit", Some(serde_json::json!(request)), true)
            .await?;
        if r.account != id {
            return Err(ActionError::Unknown);
        }
        Ok(r)
    }
}

pub fn preview_body(p: &ResetCreditPreview) -> String {
    crate::i18n::format(
        "{0}\n{1}\n만료 {2}\n영향: 해당되는 Codex 한도\n만료가 가장 빠른 리셋권 1개를 사용합니다.\n확인 유효 시각 {3}",
        &[
            format!("{} ({})", p.label, p.account),
            p.email.clone(),
            expiry(&p.grant.expires_at),
            p.expires_at.clone(),
        ],
    )
}

/// One open dialog shared by both entry points. A cancelled generation cannot reopen.
#[derive(Default)]
pub struct Consent {
    pub preview: Option<ResetCreditPreview>,
    pub account: Option<String>,
    pub busy: bool,
    pub messages: BTreeMap<String, &'static str>,
    pub unknown: BTreeMap<String, String>,
    generation: u64,
}
impl Consent {
    pub fn begin(&mut self, id: &str, refresh: bool) -> Option<u64> {
        if self.busy || (!refresh && self.unknown.contains_key(id)) {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.account = Some(id.into());
        self.preview = None;
        self.busy = true;
        self.messages.remove(id);
        Some(self.generation)
    }
    pub fn ready(&mut self, generation: u64, p: ResetCreditPreview, now: DateTime<Utc>) -> bool {
        if self.generation != generation || self.account.as_deref() != Some(&p.account) {
            return false;
        }
        self.busy = false;
        if p.preview_token.is_empty()
            || !p.inventory.grants.contains(&p.grant)
            || p.grant.status != ResetCreditStatus::Available
            || p.grant.reset_type != ResetCreditType::CodexRateLimits
            || match &p.grant.expires_at {
                ResetCreditExpiry::At { at } => {
                    !DateTime::parse_from_rfc3339(at).is_ok_and(|at| at > now)
                }
                ResetCreditExpiry::Never => false,
                ResetCreditExpiry::Unknown => true,
            }
            || p.effect != "eligible_codex_limits"
            || !inventory_line("codex", Some(&p.inventory), now).can_use
            || !DateTime::parse_from_rfc3339(&p.expires_at).is_ok_and(|at| at > now)
        {
            self.messages.insert(p.account, STALE);
            return false;
        }
        self.preview = Some(p);
        true
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.preview = None;
        self.account = None; /* in-flight dispatch remains disabled until completion */
    }
    pub fn confirm(&mut self, id: &str, now: DateTime<Utc>) -> Option<ResetCreditCommitRequest> {
        if self.busy || self.account.as_deref() != Some(id) || self.unknown.contains_key(id) {
            return None;
        }
        let p = self.preview.take()?;
        if !DateTime::parse_from_rfc3339(&p.expires_at).is_ok_and(|at| at > now) {
            self.messages.insert(id.into(), STALE);
            return None;
        }
        self.busy = true;
        // Fence immediately, including if the worker fails after dispatch.
        self.unknown.insert(id.into(), p.grant.id);
        Some(ResetCreditCommitRequest {
            preview_token: p.preview_token,
            confirm: true,
        })
    }
    pub fn finish_read(&mut self, id: &str, generation: u64, error: Option<&ActionError>) {
        self.busy = false;
        if self.generation == generation
            && let Some(e) = error
        {
            self.messages.insert(id.into(), e.message());
        }
    }
    pub fn reconciled(&mut self, id: &str, inventory: &ResetCreditInventory) {
        // Core's durable fence remains authoritative; no UI inference can bypass it.
        if inventory.state == ResetCreditState::Fresh
            && matches!(
                inventory.refusal,
                None | Some(qb_proto::reset_credits::ResetCreditRefusal::NoCredits)
            )
            && self.unknown.get(id).is_some_and(|grant| {
                inventory.grants.iter().any(|g| {
                    &g.id == grant
                        && matches!(
                            g.status,
                            ResetCreditStatus::Available | ResetCreditStatus::Redeemed
                        )
                })
            })
        {
            self.unknown.remove(id);
            self.messages.remove(id);
        }
    }
    pub fn result(&mut self, id: &str, result: &Result<ResetCreditResult, ActionError>) {
        self.busy = false;
        self.preview = None;
        let message = match result {
            Ok(r) if r.outcome == ResetCreditOutcome::Unknown => UNKNOWN,
            Err(ActionError::Stale) => {
                self.unknown.remove(id);
                STALE
            }
            Err(_) => UNKNOWN,
            Ok(r) => {
                self.unknown.remove(id);
                match r.outcome {
                    ResetCreditOutcome::Reset | ResetCreditOutcome::AlreadyRedeemed
                        if r.refresh_failed =>
                    {
                        "리셋권을 사용했습니다. 사용량·리셋권 정보는 새로고침이 필요합니다."
                    }
                    ResetCreditOutcome::Reset => "리셋권 1개를 사용했습니다.",
                    ResetCreditOutcome::AlreadyRedeemed => "이 리셋권은 이미 사용되었습니다.",
                    ResetCreditOutcome::NothingToReset => "초기화할 Codex 한도가 없습니다.",
                    ResetCreditOutcome::NoCredit => "사용 가능한 리셋권이 없습니다.",
                    ResetCreditOutcome::Unknown => UNKNOWN,
                }
            }
        };
        self.messages.insert(id.into(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qb_proto::reset_credits::*;
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    fn grant(id: &str, expires_at: ResetCreditExpiry) -> ResetCreditGrant {
        ResetCreditGrant {
            id: id.into(),
            expires_at,
            status: ResetCreditStatus::Available,
            reset_type: ResetCreditType::CodexRateLimits,
        }
    }
    fn inventory() -> ResetCreditInventory {
        ResetCreditInventory {
            state: ResetCreditState::Fresh,
            observed_at: Some(now().to_rfc3339()),
            available_count: Some(1),
            grants: vec![grant("one", ResetCreditExpiry::Never)],
            refusal: None,
        }
    }
    fn preview() -> ResetCreditPreview {
        let i = inventory();
        ResetCreditPreview {
            account: "a".into(),
            label: "synthetic account".into(),
            email: "sample@example.invalid".into(),
            preview_token: "one-time-token".into(),
            expires_at: (now() + chrono::Duration::seconds(60)).to_rfc3339(),
            grant: i.grants[0].clone(),
            inventory: i,
            effect: "eligible_codex_limits".into(),
        }
    }
    #[test]
    fn unknown_unsupported_zero_stale_and_partial_are_distinct_in_both_languages() {
        for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(language, || {
                let unknown = inventory_line("codex", None, now());
                let unsupported = inventory_line("claude", None, now());
                let mut i = inventory();
                i.available_count = Some(0);
                i.grants.clear();
                i.refusal = Some(ResetCreditRefusal::NoCredits);
                let zero = inventory_line("codex", Some(&i), now());
                i.state = ResetCreditState::Stale;
                let stale = inventory_line("codex", Some(&i), now());
                i.state = ResetCreditState::Fresh;
                i.available_count = Some(7);
                i.refusal = Some(ResetCreditRefusal::Incomplete);
                let partial = inventory_line("codex", Some(&i), now());
                let lines = [&unknown, &unsupported, &zero, &stale, &partial];
                for (a, line) in lines.iter().enumerate() {
                    assert!(!line.can_use);
                    for other in lines.iter().skip(a + 1) {
                        assert_ne!(line.text, other.text);
                    }
                }
                assert!(partial.text.contains('7'));
                assert!(!partial.text.contains("earliest") && !partial.text.contains("가장 빠른"));
            });
        }
    }
    #[test]
    fn expiry_and_completeness_match_core_exclusions_and_exact_ttl_boundary() {
        let mut i = inventory();
        let mut used = grant("used", ResetCreditExpiry::Unknown);
        used.status = ResetCreditStatus::Redeemed;
        used.reset_type = ResetCreditType::Unknown;
        let mut redeeming = used.clone();
        redeeming.id = "sending".into();
        redeeming.status = ResetCreditStatus::Redeeming;
        let mut expired = grant(
            "expired",
            ResetCreditExpiry::At {
                at: now().to_rfc3339(),
            },
        );
        expired.status = ResetCreditStatus::Unknown;
        expired.reset_type = ResetCreditType::Unknown;
        i.grants.extend([used, redeeming, expired]);
        assert!(inventory_line("codex", Some(&i), now()).can_use);
        assert!(inventory_line("codex", Some(&i), now() + chrono::Duration::seconds(299)).can_use);
        assert!(
            inventory_line(
                "codex",
                Some(&i),
                now() + chrono::Duration::milliseconds(299_999)
            )
            .can_use
        );
        assert!(!inventory_line("codex", Some(&i), now() + chrono::Duration::seconds(300)).can_use);
        i.available_count = Some(4);
        assert!(!inventory_line("codex", Some(&i), now()).can_use);
        i.available_count = Some(1);
        i.grants.push(grant("unknown", ResetCreditExpiry::Unknown));
        assert!(!inventory_line("codex", Some(&i), now()).can_use);
    }
    #[test]
    fn count_only_capped_duplicate_and_unknown_expiry_never_claim_global_earliest() {
        let mut i = inventory();
        for n in [2, 100] {
            i.available_count = Some(n);
            let line = inventory_line("codex", Some(&i), now());
            assert!(!line.can_use);
            assert!(line.text.contains(&n.to_string()));
            assert!(!line.text.contains("가장 빠른"));
        }
        i.available_count = Some(1);
        i.grants.clear();
        assert!(!inventory_line("codex", Some(&i), now()).can_use);
        i = inventory();
        i.grants.push(i.grants[0].clone());
        assert!(!inventory_line("codex", Some(&i), now()).can_use);
        i = inventory();
        i.grants[0].expires_at = ResetCreditExpiry::Unknown;
        assert!(!inventory_line("codex", Some(&i), now()).can_use);
    }
    #[test]
    fn no_expiry_and_unknown_expiry_do_not_share_copy_and_dated_earliest_uses_instants() {
        assert_ne!(
            expiry(&ResetCreditExpiry::Never),
            expiry(&ResetCreditExpiry::Unknown)
        );
        let mut i = inventory();
        i.available_count = Some(3);
        i.grants.extend([
            grant(
                "later",
                ResetCreditExpiry::At {
                    at: "2026-11-09T00:00:00Z".into(),
                },
            ),
            grant(
                "first",
                ResetCreditExpiry::At {
                    at: "2026-11-09T00:00:00+09:00".into(),
                },
            ),
        ]);
        let line = inventory_line("codex", Some(&i), now());
        assert!(line.can_use);
        assert!(line.text.contains("2026-11-09T00:00:00+09:00"));
        assert!(line.tooltip.contains("2026-11-07T10:00:00+00:00"));
    }
    #[test]
    fn cancellation_late_preview_wrong_account_and_every_new_confirmation_send_zero() {
        for _reason in ["cancel", "escape", "dismiss", "hide"] {
            let mut c = Consent::default();
            let generation = c.begin("a", false).unwrap();
            assert!(c.begin("a", false).is_none());
            c.cancel();
            c.finish_read("a", generation, None);
            assert!(!c.ready(generation, preview(), now()));
            assert!(c.confirm("a", now()).is_none());
            let generation = c.begin("a", false).unwrap();
            c.finish_read("a", generation, None);
            assert!(c.ready(generation, preview(), now()));
            assert!(c.confirm("b", now()).is_none());
            c.cancel();
            assert!(c.confirm("a", now()).is_none());
        }
    }
    #[test]
    fn affirmative_yields_one_commit_double_click_replay_and_expired_preview_yield_none() {
        let mut c = Consent::default();
        let generation = c.begin("a", false).unwrap();
        c.finish_read("a", generation, None);
        assert!(c.ready(generation, preview(), now()));
        let request = c.confirm("a", now()).unwrap();
        assert!(request.confirm);
        assert_eq!(request.preview_token, "one-time-token");
        assert!(c.confirm("a", now()).is_none());
        assert!(c.begin("a", false).is_none());
        c.result("a", &Err(ActionError::Unknown));
        assert_eq!(c.messages.get("a"), Some(&UNKNOWN));
        assert!(c.begin("a", false).is_none());
        assert!(c.begin("a", true).is_some());
        c.reconciled("a", &ResetCreditInventory::default());
        assert!(c.unknown.contains_key("a"));
        c.reconciled("a", &inventory());
        assert!(!c.unknown.contains_key("a"));
        c.finish_read("a", 0, None);
        let generation = c.begin("a", false).unwrap();
        c.finish_read("a", generation, None);
        assert!(c.ready(generation, preview(), now()));
        assert!(
            c.confirm("a", now() + chrono::Duration::seconds(60))
                .is_none()
        );
        assert_eq!(c.messages.get("a"), Some(&STALE));
    }
    #[test]
    fn result_never_invents_inventory_or_conflates_success_refresh_failure_with_unknown() {
        let mut c = Consent::default();
        c.unknown.insert("a".into(), "one".into());
        c.result(
            "a",
            &Ok(ResetCreditResult {
                account: "a".into(),
                outcome: ResetCreditOutcome::Reset,
                inventory: inventory(),
                refresh_failed: true,
            }),
        );
        assert!(!c.unknown.contains_key("a"));
        assert_ne!(c.messages.get("a"), Some(&UNKNOWN));
        assert!(c.messages["a"].contains("새로고침"));
        c.unknown.insert("a".into(), "one".into());
        c.result("a", &Err(ActionError::Stale));
        assert_eq!(c.messages.get("a"), Some(&STALE));
        assert!(c.preview.is_none());
    }
    #[test]
    fn stale_and_unknown_nested_wire_errors_remain_distinct_and_never_authorize_replay() {
        let stale = br#"{"error":{"code":"reset_credit_stale"}}"#;
        let unknown = br#"{"error":{"code":"reset_credit_unknown"}}"#;
        assert!(matches!(
            response_error(reqwest::StatusCode::CONFLICT, stale, true),
            ActionError::Stale
        ));
        assert!(matches!(
            response_error(reqwest::StatusCode::CONFLICT, unknown, true),
            ActionError::Unknown
        ));
        for status in [401, 403, 429, 500, 503] {
            assert!(matches!(
                response_error(reqwest::StatusCode::from_u16(status).unwrap(), b"{}", true),
                ActionError::Unknown
            ));
        }
        assert!(matches!(
            response_error(reqwest::StatusCode::CONFLICT, unknown, false),
            ActionError::Unknown
        ));
    }
    #[test]
    fn preview_copy_has_identity_exact_expiry_and_only_evidenced_effect_in_both_languages() {
        for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(language, || {
                let mut p = preview();
                p.account = "cx-synthetic-selected".into();
                let body = preview_body(&p);
                assert!(
                    body.contains(&p.label)
                        && body.contains(&p.account)
                        && body.contains(&p.email)
                        && body.contains(&p.expires_at)
                );
                assert!(body.contains(&expiry(&p.grant.expires_at)));
                assert!(
                    !body.contains("5h")
                        && !body.contains("7d")
                        && !body.contains(&p.preview_token)
                );
                if language == crate::i18n::UiLanguage::En {
                    assert!(body.contains("eligible Codex limits"));
                }
            });
        }
    }
}
