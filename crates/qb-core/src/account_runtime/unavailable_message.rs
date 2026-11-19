//! Message projection of actual selector gates; never selection authority.
use super::*;

impl AccountRuntime {
    /// Policy/move-guard shells already own the refusal and response envelope.
    /// Listing metadata here must not re-run selection or validate other state.
    pub(crate) async fn unavailable_message(
        &self,
        provider: Provider,
        model: Option<&str>,
        rejections: &BTreeMap<String, &'static str>,
    ) -> String {
        let store = self.store.clone();
        let accounts = crate::boxes::spawn_blocking(move || store.list())
            .await
            .ok()
            .and_then(Result::ok);
        format_accounts(
            provider,
            model,
            accounts.as_deref(),
            rejections,
            &BTreeMap::new(),
        )
    }
}

struct Row<'a> {
    id: &'a str,
    plan: Option<&'a str>,
    login_reason: Option<&'a str>,
}

pub(super) fn format_accounts(
    provider: Provider,
    model: Option<&str>,
    accounts: Option<&[Account]>,
    rejections: &BTreeMap<String, &'static str>,
    until: &BTreeMap<String, DateTime<Utc>>,
) -> String {
    let rows = accounts.map(|accounts| {
        accounts
            .iter()
            .filter(|a| a.provider == provider)
            .map(|a| Row {
                id: &a.id,
                plan: a.plan.as_deref(),
                login_reason: match &a.health {
                    Health::NeedsRelogin { reason, .. } => Some(reason.as_str()),
                    _ => None,
                },
            })
            .collect::<Vec<_>>()
    });
    format_rows(provider, model, rows.as_deref(), rejections, until)
}

// Every returned label and gate is closed. Unknown labels (even &'static str)
// are not echoed, and missing labels never become a guessed health/policy gate.
fn gate_label(gate: Option<&&'static str>) -> (&'static str, &'static str, bool) {
    match gate.copied() {
        Some("catalog_entitlement") => (
            "catalog_entitlement",
            "model not in its catalog or entitlement rejected",
            false,
        ),
        Some("disabled") => ("disabled", "disabled by user", false),
        Some("paused") => ("paused", "paused by user", false),
        Some("drain") => ("drain", "draining", false),
        Some("needs_relogin") => ("needs_relogin", "needs re-login", true),
        Some("credentials") => ("credentials", "no usable login credentials", true),
        Some("selection_credentials") => (
            "selection_credentials",
            "credential selection refused",
            false,
        ),
        Some("retry_exclusion") => ("retry_exclusion", "excluded for this retry", false),
        Some("runtime_unavailable") => ("runtime_unavailable", "runtime unavailable", false),
        Some("assignability") => ("assignability", "not assignable", false),
        Some("catalog_lookup") => ("catalog_lookup", "catalog lookup refused", false),
        Some("strict_anchor") => ("strict_anchor", "strict account anchor", false),
        Some("runtime_eligibility_or_cooling") => (
            "runtime_eligibility_or_cooling",
            "runtime eligibility or cooling hold",
            false,
        ),
        Some("outside_use_destination") => (
            "outside_use_destination",
            "destination in outside use",
            false,
        ),
        Some("discretionary_target") => (
            "discretionary_target",
            "discretionary target refused",
            false,
        ),
        Some("placement_gate") => ("placement_gate", "placement refused", false),
        Some("placement_room_partition") => (
            "placement_room_partition",
            "placement room partition",
            false,
        ),
        Some("overload_target_partition") => (
            "overload_target_partition",
            "overload target partition",
            false,
        ),
        Some("placement_pacing") => ("placement_pacing", "placement pacing", false),
        Some("placement_load_partition") => (
            "placement_load_partition",
            "placement load partition",
            false,
        ),
        Some("placement_evidence_partition") => (
            "placement_evidence_partition",
            "placement evidence partition",
            false,
        ),
        Some("return_guard") => ("return_guard", "account return guard", false),
        Some("capacity_preferred_source") => (
            "capacity_preferred_source",
            "capacity preferred source",
            false,
        ),
        Some("rebind_source") => ("rebind_source", "rebind source refused", false),
        Some("rebind_guard") => ("rebind_guard", "rebind guard", false),
        Some("rebind_move_proof") => ("rebind_move_proof", "rebind move proof refused", false),
        Some("rebind_permit") => ("rebind_permit", "rebind permit refused", false),
        Some("selection_evidence") => ("selection_evidence", "selection evidence refused", false),
        Some("admission_source") => ("admission_source", "admission source refused", false),
        Some("pin_evidence") => ("pin_evidence", "pin evidence refused", false),
        Some("store_evidence") => ("store_evidence", "store evidence refused", false),
        Some("failure_evidence") => ("failure_evidence", "failure evidence refused", false),
        Some("dispatch_admission") => ("dispatch_admission", "dispatch admission refused", false),
        Some("binding_capacity") => ("binding_capacity", "binding capacity", false),
        Some("runtime_probe") => ("runtime_probe", "authentication probe pending", false),
        Some("runtime_blocked") => ("runtime_blocked", "runtime blocked", false),
        Some("reconnect_exclusion") => {
            ("reconnect_exclusion", "excluded for this reconnect", false)
        }
        Some("socket_capacity") => ("socket_capacity", "socket capacity", false),
        Some("source_socket_capacity") => {
            ("source_socket_capacity", "source socket capacity", false)
        }
        Some("routing_policy") => ("routing_policy", "routing policy", false),
        Some("move_guard") => ("move_guard", "routing move guard", false),
        Some("placement_cap") => ("placement_cap", "placement capacity", false),
        Some("outside_use") => ("outside_use", "outside use", false),
        Some("native_eligibility") => ("native_eligibility", "native eligibility refused", false),
        Some("plan_excluded") => ("plan_excluded", "plan excluded", false),
        Some("quota") => ("quota", "quota limited", false),
        Some("quota_health") => ("quota_health", "quota health hold", false),
        Some("health_quota") => ("health_quota", "quota health hold", false),
        Some("rate") => ("rate", "rate limited", false),
        Some("auth") => ("auth", "authentication hold", false),
        Some("permission") => ("permission", "permission hold", false),
        Some("payment") => ("payment", "payment hold", false),
        Some("store_error") => ("store_error", "account store hold", false),
        Some("link_stale") => ("link_stale", "linked credentials stale", false),
        _ => ("internal", "selection refused (internal)", false),
    }
}

fn login_label(reason: &str) -> &'static str {
    match reason {
        "token_revoked" => "token revoked",
        "refresh_token_invalidated" => "refresh token invalidated",
        "refresh_token_reused" => "refresh token reused",
        "refresh_token_expired" => "refresh token expired",
        "refresh_token_revoked" => "refresh token revoked",
        "invalid_grant" => "invalid grant",
        "token_endpoint_401" => "token endpoint rejected login",
        _ => "login unusable",
    }
}

fn format_rows(
    provider: Provider,
    model: Option<&str>,
    rows: Option<&[Row<'_>]>,
    rejections: &BTreeMap<String, &'static str>,
    until: &BTreeMap<String, DateTime<Utc>>,
) -> String {
    use std::fmt::Write;
    // Preserve the historical empty-pool response exactly, including when the
    // caller knows a model. A failed store read is not an empty pool.
    if rows.is_some_and(|rows| rows.is_empty()) {
        return format!(
            "quota-baton: no usable {0} account — run quota-baton accounts login {0} --no-browser",
            provider.as_str()
        );
    }
    let target = match model {
        Some(m) if crate::catalog::valid_model_id(m) => m,
        Some(_) => "the requested model (invalid model identifier)",
        None => "the request",
    };
    let mut message = format!(
        "quota-baton: no usable {} account for {target} now",
        provider.as_str()
    );
    let Some(rows) = rows else {
        message.push_str(" — selection refused (internal): account list unavailable");
        return message;
    };
    // Check every row, including omitted rows; never infer login failure from
    // stored health or missing gate evidence. Successful empty lists qualify.
    let login_only = rows.iter().all(|r| gate_label(rejections.get(r.id)).2);
    const MAX_ROWS: usize = 16;
    for (index, row) in rows.iter().take(MAX_ROWS).enumerate() {
        message.push_str(if index == 0 { " — " } else { "; " });
        let id = if !row.id.is_empty()
            && row.id.len() <= 64
            && row
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            row.id
        } else {
            "<invalid-id>"
        };
        message.push_str(id);
        if let Some(plan) = row.plan {
            let plan = match plan {
                "free" | "plus" | "pro" | "prolite" | "team" | "business" | "enterprise"
                | "edu" | "max" => plan,
                _ => "unknown plan",
            };
            let _ = write!(message, " ({plan})");
        }
        let (gate, label, login) = gate_label(rejections.get(row.id));
        let _ = write!(message, ": {label} [gate={gate}]");
        if login && let Some(reason) = row.login_reason {
            let _ = write!(message, " ({})", login_label(reason));
        }
        // This is supplementary observed metadata, never a replacement gate.
        if let Some(until) = until.get(row.id) {
            let _ = write!(message, "; cooling/limited until {}", until.to_rfc3339());
        }
    }
    if rows.len() > MAX_ROWS {
        let _ = write!(message, "; {} more accounts omitted", rows.len() - MAX_ROWS);
    }
    if login_only {
        let _ = write!(
            message,
            " — run quota-baton accounts login {} --no-browser",
            provider.as_str()
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r32_login_hint_requires_all_actual_gates_or_known_empty_list() {
        let rows = [
            Row {
                id: "cx-a",
                plan: None,
                login_reason: Some("token_revoked"),
            },
            Row {
                id: "cx-b",
                plan: None,
                login_reason: Some("secret@example.test"),
            },
        ];
        let mut gates = BTreeMap::from([
            ("cx-a".into(), "needs_relogin"),
            ("cx-b".into(), "needs_relogin"),
        ]);
        let message = format_rows(
            Provider::Codex,
            Some("gpt-6.2-sol"),
            Some(&rows),
            &gates,
            &BTreeMap::new(),
        );
        assert!(message.contains("needs re-login [gate=needs_relogin] (token revoked)"));
        assert!(message.contains("login unusable"));
        assert!(!message.contains("secret"));
        assert!(message.contains("accounts login codex"));
        assert!(message.starts_with("quota-baton: no usable codex account"));
        assert_eq!(
            format_rows(
                Provider::Codex,
                Some("gpt-6.2-sol"),
                Some(&[]),
                &gates,
                &BTreeMap::new()
            ),
            "quota-baton: no usable codex account — run quota-baton accounts login codex --no-browser"
        );
        gates.insert("cx-b".into(), "disabled");
        assert!(
            !format_rows(Provider::Codex, None, Some(&rows), &gates, &BTreeMap::new())
                .contains("accounts login")
        );
        assert!(
            format_rows(Provider::Codex, None, Some(&[]), &gates, &BTreeMap::new())
                .contains("accounts login")
        );
        assert!(
            !format_rows(Provider::Codex, None, None, &gates, &BTreeMap::new())
                .contains("accounts login")
        );
    }

    #[test]
    fn r32_entitled_limited_account_uses_supplied_operational_deadline_and_real_gate() {
        let rows = [
            Row {
                id: "cx-763c46",
                plan: Some("prolite"),
                login_reason: None,
            },
            Row {
                id: "cx-9f1af8",
                plan: Some("free"),
                login_reason: None,
            },
        ];
        let gates = BTreeMap::from([
            ("cx-763c46".into(), "runtime_eligibility_or_cooling"),
            ("cx-9f1af8".into(), "catalog_entitlement"),
        ]);
        let until = DateTime::parse_from_rfc3339("2026-11-19T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let deadlines = BTreeMap::from([("cx-763c46".into(), until)]);
        let message = format_rows(
            Provider::Codex,
            Some("gpt-6.2-sol"),
            Some(&rows),
            &gates,
            &deadlines,
        );
        assert!(message.contains("for gpt-6.2-sol now"));
        assert!(message.contains("cx-763c46 (prolite): runtime eligibility or cooling hold [gate=runtime_eligibility_or_cooling]; cooling/limited until 2026-11-19T09:00:00+00:00"));
        assert!(message.contains("cx-9f1af8 (free): model not in its catalog"));
        assert_eq!(message.matches("cooling/limited until").count(), 1);
        assert!(!message.contains("accounts login"));
    }

    #[test]
    fn r32_non_entitled_rows_preserve_actual_gates_and_never_guess_unknown_gates() {
        let rows = [Row {
            id: "cx-free",
            plan: Some("free"),
            login_reason: None,
        }];
        let gates = BTreeMap::from([("cx-free".into(), "catalog_entitlement")]);
        let message = format_rows(
            Provider::Codex,
            Some("gpt-6.1-sol"),
            Some(&rows),
            &gates,
            &BTreeMap::new(),
        );
        assert!(message.contains("cx-free (free): model not in its catalog"));
        assert!(message.contains("[gate=catalog_entitlement]"));
        assert!(!message.contains("accounts login"));
        for gate in [
            "retry_exclusion",
            "runtime_unavailable",
            "selection_credentials",
            "disabled",
            "paused",
            "needs_relogin",
            "drain",
            "assignability",
            "catalog_lookup",
            "catalog_entitlement",
            "strict_anchor",
            "runtime_eligibility_or_cooling",
            "outside_use_destination",
            "discretionary_target",
            "placement_gate",
            "placement_room_partition",
            "overload_target_partition",
            "placement_pacing",
            "placement_cap",
            "placement_load_partition",
            "placement_evidence_partition",
            "return_guard",
            "capacity_preferred_source",
            "socket_capacity",
            "rebind_source",
            "rebind_guard",
            "rebind_move_proof",
            "rebind_permit",
            "selection_evidence",
            "admission_source",
            "pin_evidence",
            "store_evidence",
            "failure_evidence",
            "dispatch_admission",
            "binding_capacity",
            "routing_policy",
            "move_guard",
        ] {
            let gates = BTreeMap::from([("cx-free".into(), gate)]);
            let message = format_rows(
                Provider::Codex,
                Some("gpt-6.1-sol"),
                Some(&rows),
                &gates,
                &BTreeMap::new(),
            );
            assert!(message.contains(&format!("[gate={gate}]")), "{message}");
            assert!(!message.contains("[gate=internal]"), "{gate}");
        }
        for gates in [
            BTreeMap::new(),
            BTreeMap::from([("cx-free".into(), "untrusted-secret")]),
        ] {
            let message = format_rows(Provider::Codex, None, Some(&rows), &gates, &BTreeMap::new());
            assert!(message.contains("selection refused (internal) [gate=internal]"));
            assert!(!message.contains("untrusted-secret"));
            assert!(!message.contains("accounts login"));
        }
        let mut many: Vec<_> = (0..16)
            .map(|_| Row {
                id: "cx-login",
                plan: Some("secret@example.test"),
                login_reason: None,
            })
            .collect();
        many.push(Row {
            id: "cx-hidden",
            plan: None,
            login_reason: None,
        });
        let gates = BTreeMap::from([
            ("cx-login".into(), "needs_relogin"),
            ("cx-hidden".into(), "placement_cap"),
        ]);
        let message = format_rows(Provider::Codex, None, Some(&many), &gates, &BTreeMap::new());
        assert!(message.contains("1 more accounts omitted"));
        assert!(!message.contains("accounts login"));
        assert!(!message.contains("secret"));
        assert!(message.len() < 8192);
    }
}
