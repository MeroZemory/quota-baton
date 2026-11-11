//! Reset-credit transport and pure consent state. No retries, remembered consent or profile I/O.
use super::{AdminClient, ClientError};
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use qb_proto::reset_credit_action::{
    ResetCreditCommitRequest, ResetCreditOutcome, ResetCreditPreview, ResetCreditResult,
};
use qb_proto::reset_credits::{
    ClaudeResetGrant, ClaudeResetInventory, ResetCreditExpiry, ResetCreditInventory,
    ResetCreditState, ResetCreditStatus, ResetCreditType,
};
use std::collections::BTreeMap;

fn text(ko: &'static str) -> String {
    qb_tray::i18n::static_text(ko).into()
}

#[derive(Clone, Debug)]
pub struct InventoryLine {
    pub text: String,
    pub tooltip: String,
    pub can_use: bool,
}

pub fn expiry(value: &ResetCreditExpiry) -> String {
    expiry_at(value, &Local::now())
}

/// The explicit clock also supplies the display timezone (FixedOffset in tests).
fn display_time<Tz: TimeZone>(value: &str, now: &DateTime<Tz>) -> Option<String> {
    let (stamp, hint) = display_time_parts(value, now)?;
    Some(format!("{stamp} ({hint})"))
}

fn display_time_parts<Tz: TimeZone>(value: &str, now: &DateTime<Tz>) -> Option<(String, String)> {
    let at = DateTime::parse_from_rfc3339(value).ok()?;
    let local = at.with_timezone(&now.timezone());
    let stamp = local
        .naive_local()
        .format(if local.year() == now.year() {
            "%m/%d %H:%M"
        } else {
            "%Y/%m/%d %H:%M"
        })
        .to_string();
    let seconds = at
        .signed_duration_since(now.with_timezone(&Utc))
        .num_seconds();
    let magnitude = seconds.unsigned_abs();
    let hint = if magnitude >= 86_400 {
        if magnitude < 172_800 {
            text(if seconds > 0 { "1일 후" } else { "1일 전" })
        } else {
            qb_tray::i18n::format(
                if seconds > 0 {
                    "{0}일 후"
                } else {
                    "{0}일 전"
                },
                &[(magnitude / 86_400).to_string()],
            )
        }
    } else if magnitude >= 3_600 {
        if magnitude < 7_200 {
            text(if seconds > 0 {
                "1시간 후"
            } else {
                "1시간 전"
            })
        } else {
            qb_tray::i18n::format(
                if seconds > 0 {
                    "{0}시간 후"
                } else {
                    "{0}시간 전"
                },
                &[(magnitude / 3_600).to_string()],
            )
        }
    } else if magnitude >= 60 {
        qb_tray::i18n::format(
            if seconds > 0 {
                "{0}분 후"
            } else {
                "{0}분 전"
            },
            &[(magnitude / 60).to_string()],
        )
    } else {
        text(if seconds > 0 { "1분 이내" } else { "방금" })
    };
    Some((stamp, hint))
}

fn expiry_at<Tz: TimeZone>(value: &ResetCreditExpiry, now: &DateTime<Tz>) -> String {
    match value {
        ResetCreditExpiry::At { at } => {
            display_time(at, now).unwrap_or_else(|| text("만료 확인 전"))
        }
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
    inventory_line_at(provider, inventory, &now.with_timezone(&Local))
}

fn inventory_line_at<Tz: TimeZone>(
    provider: &str,
    inventory: Option<&ResetCreditInventory>,
    now: &DateTime<Tz>,
) -> InventoryLine {
    if provider == "claude" {
        return claude_inventory_line(inventory, now);
    }
    let mut line = InventoryLine {
        text: text("리셋권 확인 전"),
        tooltip: format!(
            "{}\n{}",
            text("리셋권 확인 전"),
            text("리셋권 사용을 열어 확인해 주세요.")
        ),
        can_use: false,
    };
    if provider != "codex" || inventory.is_some_and(|i| i.state == ResetCreditState::Unsupported) {
        line.text.clear();
        line.tooltip = line.text.clone();
        return line;
    }
    let Some(i) = inventory else { return line };
    let observed = i
        .observed_at
        .as_deref()
        .and_then(|at| display_time(at, now))
        .unwrap_or_else(|| text("확인 전"));
    let choice = qb_core::reset_credit_display_choice(i, now.with_timezone(&Utc));
    let reported = i
        .available_count
        .map_or_else(|| "?".into(), |n| n.to_string());
    line.can_use = choice.is_ok() && i.refusal.is_none();
    line.text = match i.available_count {
        Some(0) => text("리셋권 없음"),
        Some(_) if line.can_use => {
            let grant = choice.as_ref().expect("core proved the complete choice");
            if grant.expires_at == ResetCreditExpiry::Never {
                qb_tray::i18n::format("리셋권 {0}개 · 만료 없음", &[reported])
            } else {
                qb_tray::i18n::format(
                    "리셋권 {0}개 · 가장 빠른 만료 {1}",
                    &[reported, expiry_at(&grant.expires_at, now)],
                )
            }
        }
        Some(_) => qb_tray::i18n::format("리셋권 {0}개", &[reported]),
        None => text("리셋권 확인 전"),
    };
    let reason = i
        .refusal
        .or_else(|| choice.as_ref().err().copied())
        .map(refusal_text)
        .unwrap_or_else(|| text("만료가 가장 빠른 리셋권 1개를 사용합니다."));
    line.tooltip = format!(
        "{}\n{}\n{}",
        line.text,
        qb_tray::i18n::format("확인 시각 {0}", &[observed]),
        reason
    );
    for grant in &i.grants {
        line.tooltip.push_str(&format!(
            "\n{}",
            qb_tray::i18n::format("리셋권: {0}", &[expiry_at(&grant.expires_at, now)])
        ));
    }
    line
}

pub fn refusal_text(r: qb_proto::reset_credits::ResetCreditRefusal) -> String {
    use qb_proto::reset_credits::ResetCreditRefusal::*;
    text(match r {
        Unknown => "리셋권을 새로고침해 주세요.",
        Unsupported => "리셋권 · 지원 안 함",
        Stale => "리셋권 정보가 변경되었거나 만료되었습니다. 새 미리보기를 열어 주세요.",
        NoCredits => "사용 가능한 리셋권이 없습니다.",
        Incomplete => "리셋권 상세 정보가 불완전하여 가장 빠른 만료를 확인할 수 없습니다.",
        UnknownEligibility => "리셋권 사용 자격을 확인하지 못했습니다.",
        UnknownExpiry => "리셋권 만료를 확인하지 못했습니다.",
        Ineligible => "이 계정에서는 리셋권을 사용할 수 없습니다.",
        NotUsable => "이 리셋권은 지금 사용할 수 없습니다.",
        Malformed | DuplicateIds => "리셋권 정보가 올바르지 않습니다. 새로고침하세요.",
    })
}

pub fn claude_windows(windows: &[String]) -> String {
    let labels: Vec<_> = windows
        .iter()
        .filter_map(|w| {
            Some(text(match w.as_str() {
                "five_hour" => "5시간",
                "seven_day" => "주간",
                "seven_day_overage_included" => "추가 사용 포함 주간",
                "seven_day_opus" => "Opus 주간",
                "seven_day_sonnet" => "Sonnet 주간",
                "seven_day_cowork" => "Cowork 주간",
                "seven_day_omelette" => "모델별 주간",
                "seven_day_oauth_apps" => "연결 앱 주간",
                _ => return None,
            }))
        })
        .collect();
    if labels.is_empty() {
        text("사용 한도")
    } else {
        labels.join(" · ")
    }
}

pub fn claude_label(grant: &ClaudeResetGrant) -> String {
    if !grant.label.trim().is_empty() && grant.label != grant.id {
        return grant.label.clone();
    }
    match grant.grant_type.as_deref() {
        Some("full") => text("전체"),
        Some("session") => text("5시간"),
        Some("weekly") if grant.clears.is_empty() => text("주간"),
        _ => claude_windows(&grant.clears),
    }
}

fn claude_reason_at<Tz: TimeZone>(
    reason: Option<&str>,
    cooldown: Option<&str>,
    now: &DateTime<Tz>,
) -> String {
    if reason == Some("cooldown") {
        return cooldown.and_then(|v| display_time(v, now)).map_or_else(
            || text("잠시 후 다시 시도해 주세요."),
            |at| qb_tray::i18n::format("다시 사용할 수 있는 시각: {0}", &[at]),
        );
    }
    text(match reason.unwrap_or("unavailable") {
        "needs_limit" | "not_limited" => "한도에 도달했을 때만 사용할 수 있습니다.",
        "paused" => "이 리셋권은 일시 중지되어 있습니다.",
        "expired" => "이 리셋권은 만료되었습니다.",
        "already_used" => "이 리셋권은 이미 사용되었습니다.",
        "blocked_five_hour" => "5시간 한도 때문에 지금 사용할 수 없습니다.",
        "blocked_seven_day" => "주간 한도 때문에 지금 사용할 수 없습니다.",
        "blocked_opus" => "Opus 한도 때문에 지금 사용할 수 없습니다.",
        "blocked_sonnet" => "Sonnet 한도 때문에 지금 사용할 수 없습니다.",
        "blocked_other" | "cannot_clear" => "현재 도달한 한도를 초기화할 수 없습니다.",
        "surface" => "현재 연결 방식에서는 리셋권을 사용할 수 없습니다.",
        "cli_version" => "Claude Code를 업데이트한 후 다시 확인해 주세요.",
        "tier" | "seat" | "ineligible" => "이 계정에서는 리셋권을 사용할 수 없습니다.",
        "no_grant" => "사용 가능한 리셋권이 없습니다.",
        "authentication_required" => "계정 인증이 거절되었습니다. 갱신 상태를 확인해 주세요.",
        "rate_limited" => "요청이 많습니다. 잠시 후 다시 확인해 주세요.",
        "grant_id_required" | "not_next_grant" | "unknown_grant" => {
            "리셋권 정보가 변경되었습니다. 다시 확인해 주세요."
        }
        "stamp_indeterminate" | "reset_unconfirmed" => UNKNOWN,
        _ => "이 리셋권은 지금 사용할 수 없습니다.",
    })
}

fn claude_grant_reason<Tz: TimeZone>(
    i: &ClaudeResetInventory,
    g: &ClaudeResetGrant,
    now: &DateTime<Tz>,
) -> String {
    if !i.eligible {
        return claude_reason_at(
            i.ineligible_reason.as_deref(),
            i.cooldown_until.as_deref(),
            now,
        );
    }
    if g.paused {
        return claude_reason_at(Some("paused"), None, now);
    }
    if g.resets_left == 0 {
        return text("사용 가능한 리셋권이 없습니다.");
    }
    if g.ends_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|at| at <= now.with_timezone(&Utc))
    {
        return claude_reason_at(Some("expired"), None, now);
    }
    let reason_code = g.unavailable_reason.as_deref().or_else(|| {
        if i.cooldown_until
            .as_deref()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .is_some_and(|at| at > now.with_timezone(&Utc))
        {
            Some("cooldown")
        } else if g.use_requires_limit && i.at_limit == Some(false) {
            Some("needs_limit")
        } else {
            None
        }
    });
    let reason = claude_reason_at(reason_code, i.cooldown_until.as_deref(), now);
    if g.blocking.is_empty() {
        reason
    } else {
        format!("{reason} ({})", claude_windows(&g.blocking))
    }
}

// Full is an authoritative provider grant type, never inferred from a window list.
fn claude_effect_summary(windows: &[String], full: bool) -> String {
    if full {
        text("모든 사용 한도 (5시간 · 주간 · 모델별)")
    } else {
        claude_windows(windows)
    }
}

fn claude_uses_left(count: u64) -> String {
    if count == 1 {
        text("1회 남음")
    } else {
        qb_tray::i18n::format("{0}회 남음", &[count.to_string()])
    }
}

fn claude_expiry_at<Tz: TimeZone>(g: &ClaudeResetGrant, now: &DateTime<Tz>) -> String {
    g.ends_at
        .as_deref()
        .and_then(|v| display_time(v, now))
        .unwrap_or_else(|| text("만료 확인 전"))
}

pub fn claude_choice_detail(common: &ResetCreditInventory, g: &ClaudeResetGrant) -> String {
    let Some(i) = &common.claude else {
        return text(STALE);
    };
    let mut detail = claude_grant_detail(i, g);
    // An ineligible account already has its specific provider reason below the row.
    if i.eligible
        && g.usable_now
        && let Some(reason) = common.refusal
    {
        detail.push('\n');
        detail.push_str(&refusal_text(reason));
    }
    detail
}

pub fn claude_grant_detail(i: &ClaudeResetInventory, g: &ClaudeResetGrant) -> String {
    claude_grant_detail_at(i, g, &Local::now())
}

fn claude_grant_detail_at<Tz: TimeZone>(
    i: &ClaudeResetInventory,
    g: &ClaudeResetGrant,
    now: &DateTime<Tz>,
) -> String {
    let expiry = claude_expiry_at(g, now);
    let mut detail = qb_tray::i18n::format(
        "초기화: {0} · {1} · {2}",
        &[
            claude_effect_summary(&g.clears, g.grant_type.as_deref() == Some("full")),
            claude_uses_left(g.resets_left),
            expiry,
        ],
    );
    if !i.eligible || !g.usable_now {
        detail.push('\n');
        detail.push_str(&claude_grant_reason(i, g, now));
    }
    detail
}

/// Recommendation is display-only and uses the same core eligibility proof as the card.
pub fn claude_recommendation(i: &ResetCreditInventory, now: DateTime<Utc>) -> Option<String> {
    i.claude.as_ref()?;
    qb_core::reset_credit_display_choice(i, now)
        .ok()
        .map(|g| g.id)
}

pub fn claude_preview_title(p: &ResetCreditPreview) -> Option<String> {
    if p.grant.reset_type != ResetCreditType::ClaudeRateLimits {
        return None;
    }
    let g = p
        .inventory
        .claude
        .as_ref()?
        .grants
        .iter()
        .find(|g| g.id == p.grant.id)?;
    Some(qb_tray::i18n::format(
        "{0} 리셋권 1개 사용",
        &[claude_label(g)],
    ))
}

fn claude_inventory_line<Tz: TimeZone>(
    inventory: Option<&ResetCreditInventory>,
    now: &DateTime<Tz>,
) -> InventoryLine {
    let mut line = InventoryLine {
        text: String::new(),
        tooltip: String::new(),
        can_use: false,
    };
    let Some(common) = inventory else {
        return line;
    };
    let Some(i) = &common.claude else {
        return line;
    };
    if matches!(
        common.state,
        ResetCreditState::Unknown | ResetCreditState::Unsupported
    ) {
        return line;
    }
    line.can_use = common.state == ResetCreditState::Fresh
        && common.refusal.is_none()
        && i.eligible
        && i.grants.iter().any(|g| g.usable_now)
        && qb_core::reset_credit_display_choice(common, now.with_timezone(&Utc)).is_ok();
    if i.grants.is_empty() {
        line.text = if common.available_count == Some(0) {
            text("리셋권 없음")
        } else {
            return line;
        };
    } else if i.grants.iter().all(|g| g.resets_left == 0) {
        line.text = text("리셋권 없음");
    } else {
        let counts = i
            .grants
            .iter()
            .map(|g| {
                qb_tray::i18n::format("{0} {1}개", &[claude_label(g), g.resets_left.to_string()])
            })
            .collect::<Vec<_>>()
            .join(" · ");
        line.text = qb_tray::i18n::format("리셋권 · {0}", &[counts]);
        if let Some(at) = i
            .grants
            .iter()
            .filter(|g| g.resets_left > 0)
            .filter_map(|g| g.ends_at.as_deref())
            .filter_map(|v| DateTime::parse_from_rfc3339(v).ok())
            .min()
        {
            line.text.push_str(&qb_tray::i18n::format(
                " · 가장 빠른 만료 {0}",
                &[display_time(&at.to_rfc3339(), now).unwrap_or_else(|| text("만료 확인 전"))],
            ));
        }
    }
    if common.state == ResetCreditState::Stale {
        line.text = qb_tray::i18n::format("{0} · 오래된 정보", &[line.text]);
    }
    line.tooltip = line.text.clone();
    for g in &i.grants {
        line.tooltip.push_str(&format!(
            "\n{}: {}",
            claude_label(g),
            claude_grant_detail_at(i, g, now)
        ));
    }
    if !i.eligible {
        line.tooltip.push_str(&format!(
            "\n{}",
            claude_reason_at(
                i.ineligible_reason.as_deref(),
                i.cooldown_until.as_deref(),
                now
            )
        ));
    }
    if common.state == ResetCreditState::Stale {
        line.tooltip.push_str(&format!("\n{}", text(STALE)));
    }
    line
}

fn claude_preview_body<Tz: TimeZone>(p: &ResetCreditPreview, now: &DateTime<Tz>) -> String {
    let Some(g) = p
        .inventory
        .claude
        .as_ref()
        .and_then(|i| i.grants.iter().find(|g| g.id == p.grant.id))
    else {
        return text(STALE);
    };
    let mut lines: Vec<_> = [&p.label, &p.email]
        .into_iter()
        .filter(|v| !v.trim().is_empty() && *v != &p.account)
        .cloned()
        .collect();
    if lines.is_empty() {
        lines.push(text("Claude 계정"));
    }
    let full = g.grant_type.as_deref() == Some("full");
    lines.push(qb_tray::i18n::format(
        if full {
            "이 계정의 {0}가 초기화됩니다."
        } else {
            "이 계정의 {0} 사용 한도가 초기화됩니다."
        },
        &[claude_effect_summary(&g.clears, full)],
    ));
    lines.push(qb_tray::i18n::format(
        "사용 후 남는 횟수: {0}",
        &[g.resets_left.saturating_sub(1).to_string()],
    ));
    lines.push(qb_tray::i18n::format(
        "만료: {0}",
        &[claude_expiry_at(g, now)],
    ));
    lines.join("\n")
}

fn claude_result_body(
    r: &qb_proto::reset_credit_action::ClaudeResetResult,
    selected_full: bool,
) -> String {
    let now = Local::now();
    let mut lines = vec![if r.result == "reset" {
        qb_tray::i18n::format(
            "초기화했습니다: {0}",
            &[claude_effect_summary(&r.cleared, selected_full)],
        )
    } else {
        claude_reason_at(
            r.reason.as_deref().or(Some(&r.result)),
            r.cooldown_until.as_deref(),
            &now,
        )
    }];
    if let Some(left) = r.resets_left {
        lines.push(claude_uses_left(left));
    }
    for (key, value) in [
        ("주간 초기화 시각: {0}", &r.weekly_resets_at),
        ("다시 사용할 수 있는 시각: {0}", &r.cooldown_until),
    ] {
        if let Some(at) = value.as_deref().and_then(|v| display_time(v, &now)) {
            lines.push(qb_tray::i18n::format(key, &[at]));
        }
    }
    lines.join("\n")
}

pub const UNKNOWN: &str =
    "사용 결과를 확인하지 못했습니다. 리셋권이 이미 사용됐을 수 있으니 다시 확인해 주세요.";
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
        self.preview_reset_credit_grant(id, None).await
    }
    pub async fn preview_reset_credit_grant(
        &self,
        id: &str,
        grant_id: Option<&str>,
    ) -> Result<ResetCreditPreview, ActionError> {
        let body = grant_id.map_or_else(
            || serde_json::json!({}),
            |id| serde_json::json!({"grant_id":id}),
        );
        let p: ResetCreditPreview = self
            .reset_request(id, "/preview", Some(body), false)
            .await?;
        if p.account != id
            || !match (p.effect.as_str(), p.grant.reset_type) {
                ("eligible_codex_limits", ResetCreditType::CodexRateLimits) => {
                    p.inventory.claude.is_none()
                }
                ("claude_rate_limits", ResetCreditType::ClaudeRateLimits) => {
                    p.inventory.claude.is_some()
                }
                _ => false,
            }
            || p.preview_token.is_empty()
            || grant_id.is_some_and(|id| p.grant.id != id)
        {
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
    preview_body_at(p, &Local::now())
}

fn preview_body_at<Tz: TimeZone>(p: &ResetCreditPreview, now: &DateTime<Tz>) -> String {
    if p.grant.reset_type == ResetCreditType::ClaudeRateLimits {
        return claude_preview_body(p, now);
    }
    // The host attaches the plan, which is not present in this DTO.
    let mut lines = Vec::new();
    for identity in [&p.label, &p.email] {
        if !identity.trim().is_empty() && identity != &p.account && !lines.contains(identity) {
            lines.push(identity.clone());
        }
    }
    if lines.is_empty() {
        lines.push(text("Codex 계정"));
    }
    lines.push(match &p.grant.expires_at {
        ResetCreditExpiry::At { at } => match display_time_parts(at, now) {
            Some((stamp, hint)) => {
                if qb_tray::i18n::language() == qb_tray::i18n::UiLanguage::Ko {
                    qb_tray::i18n::format(
                        "만료가 가장 빠른 리셋권({0} 만료, {1})을 사용합니다.",
                        &[stamp, hint],
                    )
                } else {
                    qb_tray::i18n::format(
                        "만료가 가장 빠른 리셋권({0} 만료)을 사용합니다.",
                        &[format!("{stamp} ({hint})")],
                    )
                }
            }
            None => text("리셋권 만료를 확인하지 못했습니다."),
        },
        ResetCreditExpiry::Never => text("만료가 없는 리셋권 1개를 사용합니다."),
        ResetCreditExpiry::Unknown => text("리셋권 만료를 확인하지 못했습니다."),
    });
    lines.push(text(
        if p.grant.reset_type == ResetCreditType::CodexRateLimits {
            "이 계정의 Codex 사용 한도가 초기화됩니다."
        } else {
            "초기화할 사용 한도를 확인하지 못했습니다."
        },
    ));
    lines.push(
        match p.inventory.available_count.and_then(|n| n.checked_sub(1)) {
            Some(n) => qb_tray::i18n::format("사용 후 남는 리셋권: {0}개", &[n.to_string()]),
            None => text("사용 후 남는 리셋권: 확인 전"),
        },
    );
    lines.join("\n")
}

/// One open dialog shared by both entry points. A cancelled generation cannot reopen.
#[derive(Default)]
pub struct Consent {
    pub preview: Option<ResetCreditPreview>,
    pub account: Option<String>,
    pub busy: bool,
    pub messages: BTreeMap<String, &'static str>,
    pub unknown: BTreeMap<String, String>,
    pub inventories: BTreeMap<String, ResetCreditInventory>,
    pub terminal: bool,
    pub success_limits: bool,
    pub sending: bool,
    pub detail_unavailable: bool,
    pub choices: Option<ResetCreditInventory>,
    pub result_detail: Option<String>,
    // Presentation only: captured from the selected Claude preview before confirm consumes it.
    // Never used by selection, commit, generation checks or Unknown settlement.
    claude_selected_full: Option<bool>,
    // Display seed only: never enters inventory choice or confirmation authority.
    read_inventory: Option<ResetCreditInventory>,
    generation: u64,
}
impl Consent {
    pub fn offer_choices(
        &mut self,
        id: &str,
        generation: u64,
        inventory: &ResetCreditInventory,
    ) -> bool {
        if self.generation != generation
            || self.account.as_deref() != Some(id)
            || self.busy
            || self.terminal
        {
            return false;
        }
        if inventory
            .claude
            .as_ref()
            .is_none_or(|i| i.grants.len() <= 1)
        {
            return false;
        }
        self.preview = None;
        self.choices = Some(inventory.clone());
        true
    }
    pub fn can_select(&self, id: &str, grant_id: &str) -> bool {
        !self.busy
            && !self.terminal
            && self.account.as_deref() == Some(id)
            && !self.unknown.contains_key(id)
            && self
                .choices
                .as_ref()
                .filter(|i| i.state == ResetCreditState::Fresh && i.refusal.is_none())
                .and_then(|i| i.claude.as_ref())
                .is_some_and(|i| {
                    i.eligible && i.grants.iter().any(|g| g.id == grant_id && g.usable_now)
                })
    }
    pub fn project_inventory(&self, input: &str) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) else {
            return input.into();
        };
        let placeholder = serde_json::to_value(ResetCreditInventory::default())
            .expect("typed inventory serializes");
        if let Some(accounts) = value.get_mut("accounts").and_then(|v| v.as_array_mut()) {
            for account in accounts {
                let Some(id) = account.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                if self.busy
                    && !self.sending
                    && self.account.as_deref() == Some(id)
                    && account.get("reset_credits") == Some(&placeholder)
                    && let Some(inventory) = &self.read_inventory
                {
                    account["reset_credits"] =
                        serde_json::to_value(inventory).expect("typed inventory serializes");
                    continue;
                }
                let Some(inventory) = self.inventories.get(id) else {
                    continue;
                };
                let cached = inventory
                    .observed_at
                    .as_deref()
                    .and_then(|v| DateTime::parse_from_rfc3339(v).ok());
                let polled = account
                    .get("reset_credits")
                    .and_then(|v| v.get("observed_at"))
                    .and_then(|v| v.as_str())
                    .and_then(|v| DateTime::parse_from_rfc3339(v).ok());
                if cached.is_some() && (polled.is_none() || cached >= polled) {
                    account["reset_credits"] =
                        serde_json::to_value(inventory).expect("typed inventory serializes");
                }
            }
        }
        value.to_string()
    }
    pub fn operation(&self) -> u64 {
        self.generation
    }
    pub fn publish_limits(&mut self, id: &str, operation: u64, successful: bool) -> bool {
        if self.account.as_deref() == Some(id) && self.generation == operation && self.terminal {
            self.success_limits = successful;
            true
        } else {
            false
        }
    }
    pub fn begin(&mut self, id: &str, _refresh: bool) -> Option<u64> {
        if self.busy {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.account = Some(id.into());
        self.preview = None;
        self.busy = true;
        self.messages.remove(id);
        self.terminal = false;
        self.success_limits = false;
        self.sending = false;
        self.detail_unavailable = false;
        self.choices = None;
        self.result_detail = None;
        self.claude_selected_full = None;
        self.read_inventory = None;
        Some(self.generation)
    }
    pub fn begin_with_inventory(
        &mut self,
        id: &str,
        refresh: bool,
        inventory: Option<&ResetCreditInventory>,
    ) -> Option<u64> {
        let generation = self.begin(id, refresh)?;
        self.read_inventory = inventory.cloned();
        Some(generation)
    }
    pub fn ready(&mut self, generation: u64, p: ResetCreditPreview, now: DateTime<Utc>) -> bool {
        if self.generation != generation || self.account.as_deref() != Some(&p.account) {
            return false;
        }
        self.busy = false;
        self.read_inventory = None;
        if p.preview_token.is_empty()
            || !p.inventory.grants.contains(&p.grant)
            || p.grant.status != ResetCreditStatus::Available
            || if p.grant.reset_type == ResetCreditType::ClaudeRateLimits {
                p.effect != "claude_rate_limits"
                    || !inventory_line("claude", Some(&p.inventory), now).can_use
                    || p.inventory.state != ResetCreditState::Fresh
                    || p.inventory.refusal.is_some()
                    || p.inventory
                        .grants
                        .iter()
                        .any(|g| g.reset_type != ResetCreditType::ClaudeRateLimits)
                    || !p.inventory.claude.as_ref().is_some_and(|i| {
                        i.eligible && i.grants.iter().any(|g| g.id == p.grant.id && g.usable_now)
                    })
            } else {
                p.grant.reset_type != ResetCreditType::CodexRateLimits
                    || p.inventory.claude.is_some()
                    || match &p.grant.expires_at {
                        ResetCreditExpiry::At { at } => {
                            !DateTime::parse_from_rfc3339(at).is_ok_and(|at| at > now)
                        }
                        ResetCreditExpiry::Never => false,
                        ResetCreditExpiry::Unknown => true,
                    }
                    || p.effect != "eligible_codex_limits"
                    || !inventory_line("codex", Some(&p.inventory), now).can_use
            }
            || !DateTime::parse_from_rfc3339(&p.expires_at).is_ok_and(|at| at > now)
        {
            self.messages.insert(p.account, STALE);
            return false;
        }
        self.inventories
            .insert(p.account.clone(), p.inventory.clone());
        self.preview = Some(p);
        self.choices = None;
        self.detail_unavailable = false;
        true
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.preview = None;
        self.account = None; /* in-flight dispatch remains disabled until completion */
        self.choices = None;
        self.read_inventory = None;
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
        self.claude_selected_full =
            (p.grant.reset_type == ResetCreditType::ClaudeRateLimits).then(|| {
                p.inventory
                    .claude
                    .as_ref()
                    .and_then(|i| i.grants.iter().find(|g| g.id == p.grant.id))
                    .is_some_and(|g| g.grant_type.as_deref() == Some("full"))
            });
        self.busy = true;
        self.sending = true;
        // Fence immediately, including if the worker fails after dispatch.
        self.unknown.insert(id.into(), p.grant.id);
        Some(ResetCreditCommitRequest {
            preview_token: p.preview_token,
            confirm: true,
        })
    }
    pub fn finish_read(&mut self, id: &str, generation: u64, error: Option<&ActionError>) {
        self.busy = false;
        self.read_inventory = None;
        if self.generation == generation
            && let Some(e) = error
        {
            self.messages.insert(id.into(), e.message());
        }
    }
    pub fn reconciled(&mut self, id: &str, inventory: &ResetCreditInventory) {
        self.inventories.insert(id.into(), inventory.clone());
        if self.account.as_deref() == Some(id) {
            self.read_inventory = None;
        }
        // Core's durable fence remains authoritative; no UI inference can bypass it.
        if inventory.claude.is_none()
            && inventory.state == ResetCreditState::Fresh
            && matches!(
                inventory.refusal,
                None | Some(qb_proto::reset_credits::ResetCreditRefusal::NoCredits)
            )
            && self.unknown.get(id).is_some_and(|grant| {
                inventory.grants.iter().any(|g| {
                    &g.id == grant
                        && g.reset_type == ResetCreditType::CodexRateLimits
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
    /// Only a successful explicit core refresh proves Claude's durable fence settled.
    pub fn refreshed(&mut self, id: &str, provider: &str, inventory: &ResetCreditInventory) {
        self.reconciled(id, inventory);
        if provider == "claude" {
            self.unknown.remove(id);
            self.messages.remove(id);
        }
    }
    pub fn result(&mut self, id: &str, result: &Result<ResetCreditResult, ActionError>) {
        let selected_full = self.claude_selected_full.take().unwrap_or(false);
        self.busy = false;
        self.detail_unavailable = false;
        self.read_inventory = None;
        self.preview = None;
        self.choices = None;
        self.result_detail = result
            .as_ref()
            .ok()
            .filter(|r| r.outcome != ResetCreditOutcome::Unknown)
            .and_then(|r| r.claude.as_ref())
            .map(|r| claude_result_body(r, selected_full))
            .filter(|detail| detail != &text("이 리셋권은 지금 사용할 수 없습니다."));
        self.terminal = true;
        self.sending = false;
        self.success_limits = false; // Result DTO has inventory, not refreshed usage windows.
        if let Ok(r) = result {
            self.inventories.insert(id.into(), r.inventory.clone());
        }
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
                    ResetCreditOutcome::Refused => "이 리셋권은 지금 사용할 수 없습니다.",
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
            claude: None,
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
    fn quiet_counts_zero_unknown_and_hidden_unsupported_in_both_languages() {
        for language in [qb_tray::i18n::UiLanguage::Ko, qb_tray::i18n::UiLanguage::En] {
            qb_tray::i18n::with_language(language, || {
                let unknown = inventory_line("codex", None, now());
                let unsupported = inventory_line("claude", None, now());
                let mut i = inventory();
                i.available_count = Some(0);
                i.grants.clear();
                i.refusal = Some(ResetCreditRefusal::NoCredits);
                let zero = inventory_line("codex", Some(&i), now());
                i.state = ResetCreditState::Stale;
                i.refusal = None;
                let stale = inventory_line("codex", Some(&i), now());
                i.state = ResetCreditState::Fresh;
                i.available_count = Some(7);
                i.refusal = Some(ResetCreditRefusal::Incomplete);
                let partial = inventory_line("codex", Some(&i), now());
                for line in [&unknown, &unsupported, &zero, &stale, &partial] {
                    assert!(!line.can_use);
                }
                assert_eq!(unknown.text, text("리셋권 확인 전"));
                assert!(unsupported.text.is_empty() && unsupported.tooltip.is_empty());
                assert_eq!(zero.text, text("리셋권 없음"));
                assert_eq!(stale.text, zero.text);
                assert_ne!(stale.tooltip, zero.tooltip);
                assert_eq!(
                    partial.text,
                    qb_tray::i18n::format("리셋권 {0}개", &["7".into()])
                );
                i.refusal = None;
                let count_only = inventory_line("codex", Some(&i), now());
                assert_eq!(count_only.text, partial.text);
                assert!(!count_only.can_use);
                i.available_count = None;
                assert_eq!(inventory_line("codex", Some(&i), now()).text, unknown.text);
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
        let local_now = now().with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap());
        let line = inventory_line_at("codex", Some(&i), &local_now);
        assert!(line.can_use);
        assert!(line.text.contains("11/09 00:00"));
        assert!(line.tooltip.contains("11/07 19:00"));
        assert!(!line.text.contains("2026") && !line.tooltip.contains("T10:00"));
    }
    #[test]
    fn r27_korean_confirmation_uses_one_expiry_clause_and_english_bytes_stay_the_same() {
        let clock = DateTime::parse_from_rfc3339("2026-11-10T20:57:00+09:00").unwrap();
        let mut p = preview();
        p.grant.expires_at = ResetCreditExpiry::At {
            at: "2026-11-11T15:57:00Z".into(),
        };
        qb_tray::i18n::with_language(qb_tray::i18n::UiLanguage::Ko, || {
            let body = preview_body_at(&p, &clock);
            assert_eq!(
                body.lines().nth(2).unwrap(),
                "만료가 가장 빠른 리셋권(11/12 00:57 만료, 1일 후)을 사용합니다."
            );
            assert_eq!(body.matches('(').count(), 1);
            assert_eq!(body.matches(')').count(), 1);
        });
        qb_tray::i18n::with_language(qb_tray::i18n::UiLanguage::En, || {
            let expected = format!(
                "{}\n{}\nThe reset credit that expires first will be used (expires 11/12 00:57 (in 1 day)).\nThis account's Codex usage limits will be reset.\nReset credits remaining after use: 0",
                p.label, p.email
            );
            assert_eq!(preview_body_at(&p, &clock).as_bytes(), expected.as_bytes());
        });
    }

    #[test]
    fn fixed_offset_clock_formats_local_dates_relative_hints_and_local_years() {
        let clock = DateTime::parse_from_rfc3339("2026-11-07T19:00:00+09:00").unwrap();
        for language in [qb_tray::i18n::UiLanguage::Ko, qb_tray::i18n::UiLanguage::En] {
            qb_tray::i18n::with_language(language, || {
                for (raw, stamp, ko_hint, en_hint) in [
                    (
                        "2026-11-28T10:00:00Z",
                        "11/28 19:00",
                        "21일 후",
                        "in 21 days",
                    ),
                    (
                        "2026-11-07T13:00:00.1234567Z",
                        "11/07 22:00",
                        "3시간 후",
                        "in 3 hours",
                    ),
                    (
                        "2026-11-07T09:00:00Z",
                        "11/07 18:00",
                        "1시간 전",
                        "1 hour ago",
                    ),
                    ("2026-11-08T10:00:00Z", "11/08 19:00", "1일 후", "in 1 day"),
                    (
                        "2026-11-07T10:17:00Z",
                        "11/07 19:17",
                        "17분 후",
                        "in 17 min",
                    ),
                    (
                        "2026-11-07T10:00:30Z",
                        "11/07 19:00",
                        "1분 이내",
                        "in under 1 min",
                    ),
                    ("2026-11-07T10:00:00Z", "11/07 19:00", "방금", "Just now"),
                ] {
                    let hint = if language == qb_tray::i18n::UiLanguage::Ko {
                        ko_hint
                    } else {
                        en_hint
                    };
                    assert_eq!(
                        display_time(raw, &clock).unwrap(),
                        format!("{stamp} ({hint})")
                    );
                }
                assert!(
                    display_time("2027-01-01T00:00:00Z", &clock)
                        .unwrap()
                        .starts_with("2027/01/01 09:00")
                );
                let west = DateTime::parse_from_rfc3339("2026-12-31T18:00:00-05:00").unwrap();
                let stamp = display_time("2027-01-01T01:00:00Z", &west).unwrap();
                assert!(stamp.starts_with("12/31 20:00"));
                assert!(!stamp.contains("2027"));
                let east = DateTime::parse_from_rfc3339("2027-01-01T01:00:00+09:00").unwrap();
                assert!(
                    display_time("2026-12-31T10:00:00Z", &east)
                        .unwrap()
                        .starts_with("2026/12/31 19:00")
                );
                assert!(display_time("bad-provider-date", &clock).is_none());
                assert_eq!(
                    expiry_at(
                        &ResetCreditExpiry::At {
                            at: "bad-provider-date".into()
                        },
                        &clock
                    ),
                    text("만료 확인 전")
                );
            });
        }
    }
    #[test]
    fn rendered_reset_credit_copy_never_exposes_wire_dates_or_identifiers() {
        let clock = DateTime::parse_from_rfc3339("2026-11-07T19:00:00+09:00").unwrap();
        for language in [qb_tray::i18n::UiLanguage::Ko, qb_tray::i18n::UiLanguage::En] {
            qb_tray::i18n::with_language(language, || {
                for value in [
                    ResetCreditExpiry::Never,
                    ResetCreditExpiry::Unknown,
                    ResetCreditExpiry::At {
                        at: "2026-11-08T10:00:00.123456700+00:00".into(),
                    },
                    ResetCreditExpiry::At {
                        at: "invalid-wire-date".into(),
                    },
                ] {
                    let mut p = preview();
                    p.account = "cx-internal-account".into();
                    p.grant.id = "grant-internal-secret".into();
                    p.grant.expires_at = value.clone();
                    p.inventory.grants[0] = p.grant.clone();
                    let line = inventory_line_at("codex", Some(&p.inventory), &clock);
                    for rendered in [
                        line.text,
                        line.tooltip,
                        preview_body_at(&p, &clock),
                        expiry_at(&value, &clock),
                    ] {
                        assert!(!rendered.contains(&p.account));
                        assert!(!rendered.contains(&p.grant.id));
                        assert!(!rendered.contains(&p.expires_at));
                        assert!(!rendered.contains("invalid-wire-date"));
                        assert!(!rendered.contains("+00:00") && !rendered.contains("+09:00"));
                        assert!(!rendered.contains(".1234567"));
                        assert!(!rendered.as_bytes().windows(11).any(|w| {
                            w[0..4].iter().all(u8::is_ascii_digit)
                                && w[4] == b'-'
                                && w[7] == b'-'
                                && w[10] == b'T'
                        }));
                        assert!(
                            !rendered.contains("만료 만료") && !rendered.contains("expiry Never")
                        );
                    }
                }
            });
        }
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
        let read_generation = c
            .begin("a", false)
            .expect("Unknown permits read-only checking");
        assert!(
            c.confirm("a", now()).is_none(),
            "read-only entry keeps the Unknown fence"
        );
        c.finish_read("a", read_generation, None);
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
                claude: None,
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
    fn preview_copy_has_user_identity_plain_effect_remaining_count_and_no_internal_metadata() {
        for language in [qb_tray::i18n::UiLanguage::Ko, qb_tray::i18n::UiLanguage::En] {
            qb_tray::i18n::with_language(language, || {
                let mut p = preview();
                p.account = "cx-synthetic-selected".into();
                p.grant.expires_at = ResetCreditExpiry::At {
                    at: "2026-11-08T10:00:00.1234567Z".into(),
                };
                p.inventory.available_count = Some(2);
                let local_now =
                    now().with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap());
                let body = preview_body_at(&p, &local_now);
                assert!(body.contains(&p.label) && body.contains(&p.email));
                assert!(body.contains("11/08 19:00"));
                assert!(body.contains(&text("이 계정의 Codex 사용 한도가 초기화됩니다.")));
                assert!(body.contains(&qb_tray::i18n::format(
                    "사용 후 남는 리셋권: {0}개",
                    &["1".into()]
                )));
                assert!(
                    !body.contains("5h")
                        && !body.contains("7d")
                        && !body.contains(&p.preview_token)
                        && !body.contains(&p.account)
                        && !body.contains(&p.expires_at)
                        && !body.contains("2026-")
                        && !body.contains("valid until")
                        && !body.contains("유효 시각")
                );
                p.grant.reset_type = ResetCreditType::Unknown;
                p.inventory.available_count = None;
                let body = preview_body_at(&p, &local_now);
                assert!(!body.contains(&text("이 계정의 Codex 사용 한도가 초기화됩니다.")));
                assert!(body.contains(&text("사용 후 남는 리셋권: 확인 전")));
                p.label = p.account.clone();
                p.email.clear();
                p.inventory.available_count = Some(0);
                let body = preview_body_at(&p, &local_now);
                assert!(!body.contains(&p.account));
                assert!(body.starts_with(&text("Codex 계정")));
                assert!(body.contains(&text("사용 후 남는 리셋권: 확인 전")));
            });
        }
    }
}

#[cfg(test)]
mod r27_projection_tests {
    use super::*;
    use qb_proto::reset_credits::ResetCreditRefusal;

    fn count_only(observed_at: Option<&str>, count: u64) -> ResetCreditInventory {
        ResetCreditInventory {
            claude: None,
            state: ResetCreditState::Fresh,
            observed_at: observed_at.map(str::to_owned),
            available_count: Some(count),
            grants: vec![],
            refusal: Some(ResetCreditRefusal::Incomplete),
        }
    }

    fn polled(a: &ResetCreditInventory) -> serde_json::Value {
        serde_json::json!({"accounts":[
            {"id":"cx-a","provider":"codex","label":"same label","reset_credits":a},
            {"id":"cx-b","provider":"codex","reset_credits":ResetCreditInventory::default()}
        ]})
    }

    fn project(consent: &Consent, input: &serde_json::Value) -> serde_json::Value {
        serde_json::from_str(&consent.project_inventory(&input.to_string())).unwrap()
    }

    #[test]
    fn r27_pending_read_preserves_only_the_owned_core_placeholder() {
        // reset_credit_cached returns this exact Default on entry.try_lock failure.
        let input = polled(&ResetCreditInventory::default());
        for observed in [None, Some("2026-11-09T00:00:01Z")] {
            let seed = count_only(observed, 2);
            let mut consent = Consent::default();
            assert_eq!(project(&consent, &input), input);
            consent
                .begin_with_inventory("cx-a", true, Some(&seed))
                .unwrap();
            let output = project(&consent, &input);
            assert_eq!(
                output["accounts"][0]["reset_credits"],
                serde_json::to_value(&seed).unwrap()
            );
            assert_eq!(
                output["accounts"][0]["label"],
                input["accounts"][0]["label"]
            );
            assert_eq!(output["accounts"][1], input["accounts"][1]);
            assert!(
                consent.inventories.is_empty(),
                "display seed is not a result cache"
            );
            assert!(consent.begin_with_inventory("cx-b", true, None).is_none());
            assert_eq!(consent.read_inventory.as_ref(), Some(&seed));
            let mut absent = input.clone();
            absent["accounts"][0]
                .as_object_mut()
                .unwrap()
                .remove("reset_credits");
            assert_eq!(project(&consent, &absent), absent);
        }
    }

    #[test]
    fn r27_new_inventory_and_newer_polled_facts_replace_the_read_seed() {
        let seed = count_only(None, 2);
        let mut consent = Consent::default();
        consent
            .begin_with_inventory("cx-a", true, Some(&seed))
            .unwrap();
        let newer = count_only(Some("2026-11-09T00:00:02Z"), 3);
        let input = polled(&newer);
        assert_eq!(
            project(&consent, &input),
            input,
            "any real polled facts beat the pending fallback"
        );
        let refused = ResetCreditInventory {
            refusal: Some(ResetCreditRefusal::Stale),
            ..Default::default()
        };
        let input = polled(&refused);
        assert_eq!(
            project(&consent, &input),
            input,
            "only the exact Default placeholder qualifies"
        );
        let result = count_only(Some("2026-11-09T00:00:01Z"), 1);
        consent.reconciled("cx-a", &result);
        assert!(consent.read_inventory.is_none());
        let output = project(&consent, &polled(&ResetCreditInventory::default()));
        assert_eq!(output["accounts"][0]["reset_credits"]["available_count"], 1);
        assert_eq!(
            project(&consent, &polled(&newer)),
            polled(&newer),
            "newer observed poll still beats cached result"
        );
        // Even a timestamp-free Unknown response is a new result, not a lock placeholder.
        consent.reconciled("cx-a", &ResetCreditInventory::default());
        let input = polled(&ResetCreditInventory::default());
        assert_eq!(project(&consent, &input), input);
    }

    #[test]
    fn r27_read_seed_never_grants_send_authority_and_cancel_releases_it() {
        let now = DateTime::parse_from_rfc3339("2026-11-09T00:00:01Z")
            .unwrap()
            .with_timezone(&Utc);
        let seed = count_only(None, 2);
        let input = polled(&ResetCreditInventory::default());
        let mut consent = Consent::default();
        consent
            .unknown
            .insert("cx-a".into(), "uncertain-grant".into());
        let generation = consent
            .begin_with_inventory("cx-a", true, Some(&seed))
            .unwrap();
        assert!(consent.confirm("cx-a", now).is_none());
        assert!(!inventory_line("codex", Some(&seed), now).can_use);
        assert_eq!(consent.unknown["cx-a"], "uncertain-grant");
        consent.cancel();
        assert!(consent.busy && consent.read_inventory.is_none());
        assert_eq!(project(&consent, &input), input);
        consent.finish_read("cx-a", generation, None);
        let other = consent
            .begin_with_inventory("cx-b", true, Some(&seed))
            .unwrap();
        let output = project(&consent, &input);
        assert_eq!(output["accounts"][0], input["accounts"][0]);
        assert_eq!(output["accounts"][1]["reset_credits"]["available_count"], 2);
        consent.finish_read("cx-b", other, None);
        assert_eq!(project(&consent, &input), input);

        let generation = consent
            .begin_with_inventory("cx-b", true, Some(&seed))
            .unwrap();
        let grant = qb_proto::reset_credits::ResetCreditGrant {
            id: "fresh-grant".into(),
            status: ResetCreditStatus::Available,
            reset_type: ResetCreditType::CodexRateLimits,
            expires_at: ResetCreditExpiry::Never,
        };
        let preview = ResetCreditPreview {
            account: "cx-b".into(),
            label: "B".into(),
            email: "b@example.invalid".into(),
            preview_token: "fresh-token".into(),
            expires_at: (now + chrono::Duration::seconds(60)).to_rfc3339(),
            grant: grant.clone(),
            effect: "eligible_codex_limits".into(),
            inventory: ResetCreditInventory {
                claude: None,
                state: ResetCreditState::Fresh,
                observed_at: Some(now.to_rfc3339()),
                available_count: Some(1),
                grants: vec![grant],
                refusal: None,
            },
        };
        assert!(consent.ready(generation, preview.clone(), now));
        assert!(consent.read_inventory.is_none());
        assert!(consent.confirm("cx-a", now).is_none());
        assert!(consent.confirm("cx-b", now).unwrap().confirm);
        assert!(consent.sending && consent.busy);
        assert!(consent.confirm("cx-b", now).is_none());
        assert_eq!(consent.unknown["cx-b"], "fresh-grant");
        consent.inventories.clear();
        assert_eq!(
            project(&consent, &input),
            input,
            "send does not reuse a read seed"
        );
        consent.cancel();
        assert_eq!(consent.unknown["cx-b"], "fresh-grant");
        assert!(!consent.ready(generation, preview, now));
    }

    #[test]
    fn r27_result_limits_wait_for_same_operation_dashboard_publication() {
        let mut consent = Consent::default();
        let operation = consent.begin("cx-a", false).unwrap();
        consent.result(
            "cx-a",
            &Ok(ResetCreditResult {
                claude: None,
                account: "cx-a".into(),
                outcome: ResetCreditOutcome::Reset,
                inventory: ResetCreditInventory::default(),
                refresh_failed: false,
            }),
        );
        assert!(
            !consent.success_limits,
            "commit DTO contains no usage windows"
        );
        assert!(!consent.publish_limits("cx-b", operation, true));
        assert!(consent.publish_limits("cx-a", operation, true));
        assert!(consent.success_limits);
        let replacement = consent.begin("cx-a", true).unwrap();
        assert_ne!(operation, replacement);
        assert!(!consent.publish_limits("cx-a", operation, true));
        assert!(!consent.success_limits);
        consent.cancel();
        assert!(!consent.publish_limits("cx-a", replacement, true));
    }
    #[test]
    fn r27_refresh_inventory_immediately_reprojects_menu_and_preserves_other_accounts() {
        let inventory = ResetCreditInventory {
            claude: None,
            state: ResetCreditState::Fresh,
            observed_at: Some("2026-11-09T00:00:01Z".into()),
            available_count: Some(2),
            grants: vec![],
            refusal: Some(ResetCreditRefusal::Incomplete),
        };
        let mut consent = Consent::default();
        consent.reconciled("cx-a", &inventory);
        let input = r#"{"accounts":[{"id":"cx-a","provider":"codex","reset_credits":{"state":"stale","observed_at":"2026-11-09T00:00:00Z","available_count":1,"grants":[],"refusal":"stale"}},{"id":"cx-b","provider":"codex"}]}"#;
        let output: serde_json::Value =
            serde_json::from_str(&consent.project_inventory(input)).unwrap();
        assert_eq!(output["accounts"][0]["reset_credits"]["available_count"], 2);
        assert_eq!(output["accounts"][0]["reset_credits"]["state"], "fresh");
        assert!(output["accounts"][1].get("reset_credits").is_none());
        let newer = input.replace("00:00:00Z", "00:00:02Z");
        let output: serde_json::Value =
            serde_json::from_str(&consent.project_inventory(&newer)).unwrap();
        assert_eq!(output["accounts"][0]["reset_credits"]["available_count"], 1);
    }
}

#[cfg(test)]
#[path = "reset_credits/r28_tests.rs"]
mod r28_tests;
