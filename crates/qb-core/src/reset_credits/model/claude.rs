//! Bounded native Claude facts. Parsing never grants authority to mutate.
use super::*;
use qb_proto::reset_credit_action::ClaudeResetResult;
use qb_proto::reset_credits::{ClaudeResetGrant, ClaudeResetInventory};
use std::collections::BTreeMap;

const MAX_ROWS: usize = 256;
const WINDOWS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_overage_included",
    "seven_day_opus",
    "seven_day_sonnet",
    "seven_day_cowork",
    "seven_day_omelette",
    "seven_day_oauth_apps",
];
const INELIGIBLE: &[&str] = &[
    "config_off",
    "tier",
    "seat",
    "mobile",
    "surface",
    "cli_version",
    "no_grant",
    "tenure",
    "other_experiment",
    "unavailable",
    "unknown",
];
const UNAVAILABLE: &[&str] = &[
    "cooldown",
    "blocked_five_hour",
    "blocked_seven_day",
    "blocked_opus",
    "blocked_sonnet",
    "blocked_other",
    "cannot_clear",
    "needs_limit",
    "unavailable",
];
const RESULTS: &[&str] = &[
    "reset",
    "already_used",
    "not_limited",
    "cooldown",
    "ineligible",
    "unavailable",
];
const REASONS: &[&str] = &[
    "paused",
    "expired",
    "unknown_grant",
    "not_next_grant",
    "grant_id_required",
    "not_limited",
    "already_used",
    "cooldown",
    "stamp_indeterminate",
    "reset_unconfirmed",
];

fn native_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn text(value: Option<&Value>, max: usize) -> Result<Option<String>, Error> {
    let Some(s) = value.and_then(Value::as_str) else {
        return Ok(None);
    };
    if s.len() > max || s.chars().any(char::is_control) {
        return Err(Error::Malformed);
    }
    Ok(Some(s.to_owned()))
}

fn enum_text(value: Option<&Value>, known: &[&str], fallback: Option<&str>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| known.contains(s))
                .or(fallback)?
                .to_owned(),
        ),
    }
}

fn windows(value: Option<&Value>) -> Result<Vec<String>, Error> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if rows.len() > MAX_ROWS {
        return Err(Error::Malformed);
    }
    Ok(rows
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| WINDOWS.contains(s))
        .map(str::to_owned)
        .collect())
}

fn flag(row: &Value, key: &str, default: bool) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(default)
}

// JS's integer schema also accepts JSON spellings such as 1.0 and 1e2.
// Keep those native semantics without saturating an out-of-range float cast.
fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let n = value.as_f64()?;
        ((0.0..18_446_744_073_709_551_616.0).contains(&n) && n.fract() == 0.0).then_some(n as u64)
    })
}

fn grant(row: &Value) -> Result<ClaudeResetGrant, Error> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| native_id(id))
        .ok_or(Error::Malformed)?;
    let resets_left = row
        .get("resets_left")
        .and_then(integer)
        .ok_or(Error::Malformed)?;
    let mut percent_used = BTreeMap::new();
    if let Some(map) = row.get("percent_used").and_then(Value::as_object) {
        if map.len() > MAX_ROWS {
            return Err(Error::Malformed);
        }
        for (key, value) in map {
            if WINDOWS.contains(&key.as_str()) && integer(value).is_some_and(|v| v <= 100) {
                percent_used.insert(key.clone(), value.as_number().unwrap().clone());
            }
        }
    }
    Ok(ClaudeResetGrant {
        id: id.to_owned(),
        label: text(row.get("label"), 256)?.unwrap_or_default(),
        grant_type: enum_text(row.get("type"), &["session", "weekly", "full"], None),
        resets_total: Some(row.get("resets_total").and_then(integer).unwrap_or(0)),
        resets_left,
        starts_at: text(row.get("starts_at"), 128)?,
        ends_at: text(row.get("ends_at"), 128)?,
        clears: windows(row.get("clears"))?,
        blocking: windows(row.get("blocking"))?,
        paused: flag(row, "paused", false),
        usable_now: flag(row, "usable_now", false),
        use_requires_limit: flag(row, "use_requires_limit", true),
        gives_back_weekly: flag(row, "gives_back_weekly", false),
        percent_used,
        unavailable_reason: enum_text(row.get("unavailable_reason"), UNAVAILABLE, None),
    })
}

/// Accepts the complete usage response, including its optional cedar_ember block.
pub(crate) fn parse_details(
    value: &Value,
    now: DateTime<Utc>,
) -> Result<ResetCreditInventory, Error> {
    value.as_object().ok_or(Error::Malformed)?;
    let Some(block) = value.get("cedar_ember").filter(|v| !v.is_null()) else {
        return Ok(ResetCreditInventory::default());
    };
    let Some(eligible) = block.get("eligible").and_then(Value::as_bool) else {
        return Ok(ResetCreditInventory::default());
    };
    let mut common = Vec::new();
    let mut grants = Vec::new();
    let mut ids = HashSet::new();
    let mut count = 0u64;
    if let Some(rows) = block.get("grants").and_then(Value::as_array) {
        if rows.len() > MAX_ROWS {
            return Err(Error::Malformed);
        }
        for row in rows {
            // Match native per-entry recovery; never manufacture a zero grant.
            let Ok(parsed) = grant(row) else { continue };
            if !ids.insert(parsed.id.clone()) {
                return Err(Error::DuplicateIds);
            }
            count = count
                .checked_add(parsed.resets_left)
                .ok_or(Error::Malformed)?;
            // Native normalizes missing/null/invalid-type dates to null.
            let expires_at = parsed
                .ends_at
                .as_ref()
                .map(|at| ResetCreditExpiry::At { at: at.clone() })
                .unwrap_or(ResetCreditExpiry::Never);
            common.push(ResetCreditGrant {
                id: parsed.id.clone(),
                expires_at,
                status: if parsed.resets_left > 0 {
                    ResetCreditStatus::Available
                } else {
                    ResetCreditStatus::Redeemed
                },
                reset_type: ResetCreditType::ClaudeRateLimits,
            });
            grants.push(parsed);
        }
    }
    let next_grant_id = block
        .get("next_grant_id")
        .and_then(Value::as_str)
        .filter(|id| native_id(id) && ids.contains(*id))
        .map(str::to_owned);
    let mut inventory = ResetCreditInventory {
        state: ResetCreditState::Fresh,
        observed_at: Some(now.to_rfc3339()),
        available_count: Some(count),
        grants: common,
        refusal: None,
        claude: Some(ClaudeResetInventory {
            eligible,
            ineligible_reason: enum_text(
                block.get("ineligible_reason"),
                INELIGIBLE,
                Some("unknown"),
            ),
            at_limit: Some(flag(block, "at_limit", false)),
            exhausted: windows(block.get("exhausted"))?,
            next_grant_id,
            weekly_resets_at: text(block.get("weekly_resets_at"), 128)?,
            cooldown_until: text(block.get("cooldown_until"), 128)?,
            grants,
        }),
    };
    inventory.refusal = choose(&inventory, None, now).err();
    Ok(inventory)
}

fn usable(
    grant: &ClaudeResetGrant,
    common: &ResetCreditGrant,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, Error> {
    if grant.resets_left == 0 {
        return Err(Error::NoCredits);
    }
    if !grant.usable_now
        // Confirmation needs a known effect, even for deserialized inventories.
        || !grant.clears.iter().any(|window| WINDOWS.contains(&window.as_str()))
        || grant.paused
        || !grant.blocking.is_empty()
        || grant.unavailable_reason.is_some()
        || common.status != ResetCreditStatus::Available
        || common.reset_type != ResetCreditType::ClaudeRateLimits
    {
        return Err(Error::NotUsable);
    }
    if let Some(start) = &grant.starts_at {
        if parse_timestamp(start).map_err(|_| Error::NotUsable)? > now {
            return Err(Error::NotUsable);
        }
    }
    match &common.expires_at {
        ResetCreditExpiry::At { at } => {
            if grant.ends_at.as_ref() != Some(at) {
                return Err(Error::UnknownExpiry);
            }
            // Core guard beyond the native string schema: an unparseable
            // expiry string cannot prove current validity for confirmation.
            let at = parse_timestamp(at).map_err(|_| Error::UnknownExpiry)?;
            if at <= now {
                return Err(Error::NotUsable);
            }
            Ok(Some(at))
        }
        ResetCreditExpiry::Never if grant.ends_at.is_none() => Ok(None),
        _ => Err(Error::UnknownExpiry),
    }
}

pub(crate) fn choose<'a>(
    inventory: &'a ResetCreditInventory,
    id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<&'a ResetCreditGrant, Error> {
    match inventory.state {
        ResetCreditState::Unknown => return Err(Error::Unknown),
        ResetCreditState::Unsupported => return Err(Error::Unsupported),
        ResetCreditState::Stale => return Err(Error::Stale),
        ResetCreditState::Fresh => {}
    }
    let observed = parse_timestamp(inventory.observed_at.as_deref().ok_or(Error::Unknown)?)?;
    if observed > now || now.signed_duration_since(observed).num_seconds() >= INVENTORY_TTL_SECONDS
    {
        return Err(Error::Stale);
    }
    let facts = inventory.claude.as_ref().ok_or(Error::Unknown)?;
    if !facts.eligible {
        return Err(Error::Ineligible);
    }
    if facts.grants.len() > MAX_ROWS || inventory.grants.len() != facts.grants.len() {
        return Err(Error::Incomplete);
    }
    let mut ids = HashSet::new();
    for g in &facts.grants {
        if !native_id(&g.id) {
            return Err(Error::Malformed);
        }
        if !ids.insert(&g.id) {
            return Err(Error::DuplicateIds);
        }
    }
    ids.clear();
    for g in &inventory.grants {
        if !ids.insert(&g.id) {
            return Err(Error::DuplicateIds);
        }
        if !facts.grants.iter().any(|f| f.id == g.id) {
            return Err(Error::Incomplete);
        }
    }
    if facts.grants.iter().all(|g| g.resets_left == 0) {
        return Err(Error::NoCredits);
    }
    if let Some(until) = &facts.cooldown_until {
        if parse_timestamp(until).map_err(|_| Error::NotUsable)? > now {
            return Err(Error::NotUsable);
        }
    }
    let resolve = |id: &str| {
        let detail = facts
            .grants
            .iter()
            .find(|g| g.id == id)
            .ok_or(Error::NotUsable)?;
        let common = inventory
            .grants
            .iter()
            .find(|g| g.id == id)
            .ok_or(Error::Incomplete)?;
        usable(detail, common, now).map(|expiry| (common, expiry))
    };
    if let Some(id) = id {
        return resolve(id).map(|(g, _)| g);
    }
    if let Some(next) = &facts.next_grant_id {
        if let Ok((g, _)) = resolve(next) {
            return Ok(g);
        }
    }
    let mut candidates: Vec<_> = facts
        .grants
        .iter()
        .filter_map(|g| resolve(&g.id).ok())
        .collect();
    candidates.sort_by(|(a, ae), (b, be)| {
        match (ae, be) {
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.id.cmp(&b.id))
    });
    candidates.first().map(|(g, _)| *g).ok_or_else(|| {
        if facts
            .grants
            .iter()
            .any(|g| matches!(resolve(&g.id), Err(Error::UnknownExpiry)))
        {
            Error::UnknownExpiry
        } else {
            Error::NotUsable
        }
    })
}

pub(crate) fn parse_result(value: &Value) -> Result<ClaudeResetResult, Error> {
    value.as_object().ok_or(Error::Malformed)?;
    let reason = match value.get("reason") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| INELIGIBLE.contains(s) || REASONS.contains(s))
                .unwrap_or("unknown")
                .to_owned(),
        ),
    };
    Ok(ClaudeResetResult {
        result: value
            .get("result")
            .and_then(Value::as_str)
            .filter(|s| RESULTS.contains(s))
            .unwrap_or("unavailable")
            .to_owned(),
        reason,
        resets_left: value.get("resets_left").and_then(integer),
        cleared: windows(value.get("cleared"))?,
        weekly_resets_at: text(value.get("weekly_resets_at"), 128)?,
        cooldown_until: text(value.get("cooldown_until"), 128)?,
    })
}

#[cfg(test)]
mod tests;
