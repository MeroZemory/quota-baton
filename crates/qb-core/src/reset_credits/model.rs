//! Pure parsing/selection for the pinned Codex backend details schema.
//! No credentials, I/O, paid-credit changes, or quota-window changes belong here.

use chrono::{DateTime, Utc};
use qb_proto::reset_credits::{
    ResetCreditExpiry, ResetCreditGrant, ResetCreditInventory, ResetCreditRefusal,
    ResetCreditState, ResetCreditStatus, ResetCreditType,
};
use serde_json::Value;
use std::collections::HashSet;

pub(crate) mod claude;

pub(crate) type Error = ResetCreditRefusal;

/// At the boundary a new read is required. Preview validity is enforced separately.
pub(crate) const INVENTORY_TTL_SECONDS: i64 = 300;

fn timestamp(value: &Value) -> Result<DateTime<Utc>, Error> {
    parse_timestamp(value.as_str().ok_or(Error::Malformed)?)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|_| Error::Malformed)
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}

/// Parses the full usage response's optional summary. A summary never proves
/// detail completeness. Missing/null summary or count is unknown, never zero.
pub(crate) fn parse_summary(
    value: &Value,
    now: DateTime<Utc>,
) -> Result<ResetCreditInventory, Error> {
    value.as_object().ok_or(Error::Malformed)?;
    let Some(summary) = value
        .get("rate_limit_reset_credits")
        .filter(|v| !v.is_null())
    else {
        return Ok(ResetCreditInventory::default());
    };
    summary.as_object().ok_or(Error::Malformed)?;
    // A summary must not become a details response through unexpected fields.
    parse_details(
        &serde_json::json!({"available_count": summary.get("available_count")}),
        now,
    )
}

/// Parses GET details, not the usage-response wrapper. Unknown provider strings
/// are reduced to finite unknown facts; titles/descriptions are never retained.
pub(crate) fn parse_details(
    value: &Value,
    now: DateTime<Utc>,
) -> Result<ResetCreditInventory, Error> {
    value.as_object().ok_or(Error::Malformed)?;
    let count = match value.get("available_count") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .and_then(|count| u64::try_from(count).ok())
                .ok_or(Error::Malformed)?,
        ),
    };
    let mut grants = Vec::new();
    let mut ids = HashSet::new();
    if let Some(rows) = value.get("credits") {
        for row in rows.as_array().ok_or(Error::Malformed)? {
            row.as_object().ok_or(Error::Malformed)?;
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .ok_or(Error::Malformed)?;
            if !valid_id(id) {
                return Err(Error::Malformed);
            }
            if !ids.insert(id) {
                return Err(Error::DuplicateIds);
            }
            // Backend dates are RFC3339 strings, never numeric epochs.
            let granted_at = timestamp(row.get("granted_at").ok_or(Error::Malformed)?)?;
            if granted_at > now {
                return Err(Error::Malformed);
            }
            let expires_at = match row.get("expires_at") {
                None => ResetCreditExpiry::Unknown,
                Some(Value::Null) => ResetCreditExpiry::Never,
                Some(value) => ResetCreditExpiry::At {
                    at: timestamp(value)?.to_rfc3339(),
                },
            };
            let status = match row
                .get("status")
                .and_then(Value::as_str)
                .ok_or(Error::Malformed)?
            {
                "available" => ResetCreditStatus::Available,
                "redeeming" => ResetCreditStatus::Redeeming,
                "redeemed" => ResetCreditStatus::Redeemed,
                _ => ResetCreditStatus::Unknown,
            };
            let reset_type = match row
                .get("reset_type")
                .and_then(Value::as_str)
                .ok_or(Error::Malformed)?
            {
                "codex_rate_limits" => ResetCreditType::CodexRateLimits,
                _ => ResetCreditType::Unknown,
            };
            grants.push(ResetCreditGrant {
                id: id.to_owned(),
                expires_at,
                status,
                reset_type,
            });
        }
    }
    let mut inventory = ResetCreditInventory {
        state: if count.is_some() {
            ResetCreditState::Fresh
        } else {
            ResetCreditState::Unknown
        },
        observed_at: Some(now.to_rfc3339()),
        available_count: count,
        grants,
        refusal: None,
        claude: None,
    };
    inventory.refusal = choose(&inventory, now).err();
    Ok(inventory)
}

/// Returns the exact borrowed grant, never a substitute ID or aggregate request.
/// The caller owns account binding, preview TTL and durable mutation fencing.
/// Rechecks expiry/completeness at `now`, including for deserialized cached DTOs.
pub(crate) fn choose(
    inventory: &ResetCreditInventory,
    now: DateTime<Utc>,
) -> Result<&ResetCreditGrant, Error> {
    if inventory.claude.is_some() {
        return claude::choose(inventory, None, now);
    }
    match inventory.state {
        ResetCreditState::Unknown => return Err(Error::Unknown),
        ResetCreditState::Unsupported => return Err(Error::Unsupported),
        ResetCreditState::Stale => return Err(Error::Stale),
        ResetCreditState::Fresh => {}
    }
    let observed = inventory.observed_at.as_deref().ok_or(Error::Unknown)?;
    let observed = parse_timestamp(observed)?;
    if observed > now || now.signed_duration_since(observed).num_seconds() >= INVENTORY_TTL_SECONDS
    {
        return Err(Error::Stale);
    }
    let count = inventory.available_count.ok_or(Error::Unknown)?;
    let mut ids = HashSet::new();
    let mut eligible = Vec::new();
    for grant in &inventory.grants {
        if !valid_id(&grant.id) {
            return Err(Error::Malformed);
        }
        if !ids.insert(&grant.id) {
            return Err(Error::DuplicateIds);
        }
        let expiry = match &grant.expires_at {
            ResetCreditExpiry::At { at } => Some(parse_timestamp(at)?),
            ResetCreditExpiry::Never | ResetCreditExpiry::Unknown => None,
        };
        if matches!(
            grant.status,
            ResetCreditStatus::Redeeming | ResetCreditStatus::Redeemed
        ) || expiry.is_some_and(|at| at <= now)
        {
            continue;
        }
        if grant.status == ResetCreditStatus::Unknown
            || grant.reset_type == ResetCreditType::Unknown
        {
            return Err(Error::UnknownEligibility);
        }
        if grant.expires_at == ResetCreditExpiry::Unknown {
            return Err(Error::UnknownExpiry);
        }
        eligible.push((grant, expiry));
    }
    if u64::try_from(eligible.len()).map_err(|_| Error::Malformed)? != count {
        return Err(Error::Incomplete);
    }
    eligible.sort_by(|(a, a_expiry), (b, b_expiry)| {
        match (a_expiry, b_expiry) {
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.id.cmp(&b.id))
    });
    eligible
        .first()
        .map(|(grant, _)| *grant)
        .ok_or(Error::NoCredits)
}

#[cfg(test)]
mod tests;
