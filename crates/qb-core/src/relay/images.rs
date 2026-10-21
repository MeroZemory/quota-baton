//! Image failures must never enter the responses health or token-refresh paths.
use super::*;
use axum::body::Body;
use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(super) struct ImageState {
    // One entry per currently stored account, never per session or image.
    accounts: BTreeMap<String, ImageHealth>,
}

struct ImageHealth {
    generation: u64,
    credential: [u8; 32],
    exhausted: Option<chrono::DateTime<chrono::Utc>>,
    incapable: bool,
}

impl ImageState {
    fn observe(&mut self, id: &str, generation: u64, credential: [u8; 32]) -> &mut ImageHealth {
        let health = self.accounts.entry(id.into()).or_insert(ImageHealth {
            generation,
            credential,
            exhausted: None,
            incapable: false,
        });
        if generation >= health.generation {
            if health.credential != credential {
                health.credential = credential;
                health.incapable = false;
            }
            health.generation = generation;
        }
        health
    }

    fn failure(
        &mut self,
        id: &str,
        generation: u64,
        credential: [u8; 32],
        auth: bool,
        reset: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let health = self.observe(id, generation, credential);
        if health.credential == credential {
            health.incapable |= auth;
        }
        if let Some(at) = reset {
            health.exhausted = Some(health.exhausted.map_or(at, |old| old.max(at)));
        }
    }
}

fn credential_key(tokens: &crate::accounts::Tokens) -> [u8; 32] {
    Sha256::digest(tokens.access_token.as_bytes()).into()
}

pub(super) fn matches(method: &Method, path: &str) -> bool {
    method == Method::POST && matches!(path, "/v1/images/generations" | "/v1/images/edits")
}

fn eligible(a: &Account) -> bool {
    let now = chrono::Utc::now();
    a.provider == Provider::Codex
        && a.user_state == UserState::Active
        && !a.drain
        && a.payment_until.is_none_or(|until| until <= now)
        && (matches!(
            a.health,
            Health::Cooling {
                reason: crate::accounts::HealthReason::Quota,
                ..
            }
        ) || a.eligible_at(Some("gpt-image-2"), now))
        && !a.token_meta.refresh_uncertain
        && a.token_meta.expires_at.is_none_or(|at| at > now)
        && a.plan.as_deref().is_some_and(|p| {
            matches!(
                p,
                "plus" | "pro" | "team" | "business" | "enterprise" | "edu" | "go"
            )
        })
}

enum EditAdmission {
    TooLarge,
    Memory,
}

// Charge capacity growth, not incoming frame count. Geometric growth bounds
// reservation records to O(log(cap)); 128 extra bytes charge each record and
// its vector slot. Four capacity copies cover inspection and parser storage.
fn extend_edit(
    raw: &mut Vec<u8>,
    reservations: &mut Vec<crate::guard::Reservation>,
    chunk: &[u8],
    cap: usize,
    budget: &Arc<crate::guard::InflightBudget>,
) -> Result<(), EditAdmission> {
    if chunk.is_empty() {
        return Ok(());
    }
    let needed = raw
        .len()
        .checked_add(chunk.len())
        .filter(|n| *n <= cap)
        .ok_or(EditAdmission::TooLarge)?;
    if needed > raw.capacity() {
        let capacity = needed
            .max(4096)
            .checked_next_power_of_two()
            .unwrap_or(cap)
            .min(cap);
        let charge = capacity
            .saturating_sub(raw.capacity())
            .saturating_mul(4)
            .saturating_add(128);
        let reservation = budget.try_reserve(charge).ok_or(EditAdmission::Memory)?;
        raw.try_reserve_exact(capacity - raw.len())
            .map_err(|_| EditAdmission::Memory)?;
        reservations.push(reservation);
    }
    raw.extend_from_slice(chunk);
    Ok(())
}

fn error(completion: &mut Completion, edit: bool, status: StatusCode, reason: &str) -> Response {
    let operation = if edit { "edit" } else { "generation" };
    completion.outcome(status.as_u16(), &format!("image_{operation}_{reason}"));
    error_response(edit, status, reason)
}

pub(super) fn timeout(edit: bool) -> Response {
    error_response(edit, StatusCode::GATEWAY_TIMEOUT, "transport_error")
}

fn error_response(edit: bool, status: StatusCode, reason: &str) -> Response {
    (status, Json(error_body(edit, reason))).into_response()
}

fn error_body(edit: bool, reason: &str) -> Value {
    let operation = if edit { "edit" } else { "generation" };
    let action = match reason {
        "no_eligible_account" => "사용 가능한 유료 Codex 계정을 확인해 주세요.",
        "account_unavailable" => {
            "전송 직전 계정 상태가 변경되었습니다. 활성 상태와 로그인 정보를 확인해 주세요."
        }
        "bound_account_unavailable" => "연결된 이미지 제작 계정을 확인해 주세요.",
        "auth_rejected" => "계정의 이미지 접근 권한을 확인해 주세요. 토큰은 변경하지 않았습니다.",
        "quota_exhausted" => "이미지 한도 초기화 후 다시 시도해 주세요.",
        "endpoint_unsupported" => "계정의 이미지 API 지원 여부를 확인해 주세요.",
        "reference_unknown" => {
            "제작 계정을 확인할 수 없는 서버 이미지 참조입니다. 로컬 이미지 데이터를 사용해 주세요."
        }
        "body_uninspectable" => {
            "편집 입력 크기·형식·인코딩을 확인하고 로컬 이미지 데이터를 사용해 주세요."
        }
        "memory_guard" => "메모리 여유를 확보한 뒤 다시 시도해 주세요.",
        _ => "이미지 서버 연결을 확인한 뒤 직접 다시 시도해 주세요.",
    };
    let code = format!("image_{operation}_{reason}");
    let message = format!(
        "이미지 {} 실패: {action}",
        if edit { "편집" } else { "생성" }
    );
    serde_json::json!({"error": {
        "message": message, "type": "image_relay_error", "param": null, "code": code
    }})
}

// Codex ImageData returns generation_id, whereas ImageReference::File edits take
// file_id. The client does not convert between these namespaces: its generated
// images are edited via inline b64_json; file_id can instead come from history.
// A generation_id registry cannot prove a file_id's producer. Until an upstream
// response supplies that mapping, server references fail closed, even if bound.
fn local_json(raw: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InlineImage {
        image_url: String,
    }
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Edit {
        images: Vec<InlineImage>,
        mask: Option<InlineImage>,
        prompt: Option<serde::de::IgnoredAny>,
        model: Option<serde::de::IgnoredAny>,
        n: Option<serde::de::IgnoredAny>,
        size: Option<serde::de::IgnoredAny>,
        quality: Option<serde::de::IgnoredAny>,
        background: Option<serde::de::IgnoredAny>,
        output_format: Option<serde::de::IgnoredAny>,
        output_compression: Option<serde::de::IgnoredAny>,
        moderation: Option<serde::de::IgnoredAny>,
        user: Option<serde::de::IgnoredAny>,
    }
    fn inline(image: &InlineImage) -> bool {
        image.image_url.starts_with("data:image/") && image.image_url.contains(";base64,")
    }
    // Derived fields reject duplicate keys as well as server/unknown references.
    serde_json::from_slice::<Edit>(raw).ok().is_some_and(|v| {
        !v.images.is_empty() && v.images.iter().all(inline) && v.mask.as_ref().is_none_or(inline)
    })
}

fn local_multipart(raw: &[u8], content_type: &str, model: &mut Option<String>) -> bool {
    let mut parameters = content_type.split(';').skip(1);
    let Some(parameter) = parameters.next() else {
        return false;
    };
    if parameters.next().is_some() {
        return false;
    }
    let Some(boundary) = parameter.trim().strip_prefix("boundary=") else {
        return false;
    };
    let boundary = if boundary.starts_with('"') {
        let Some(value) = boundary.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
            return false;
        };
        value
    } else {
        boundary
    };
    if boundary.is_empty()
        || boundary.len() > 70
        || !boundary
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=?".contains(&b))
    {
        return false;
    }
    let marker = format!("--{boundary}");
    if !raw.starts_with(marker.as_bytes()) {
        return false;
    }
    let mut rest = &raw[marker.len()..];
    let mut image = false;
    let mut model_seen = false;
    loop {
        if rest == b"--\r\n" || rest == b"--" {
            return image;
        }
        let Some(part) = rest.strip_prefix(b"\r\n") else {
            return false;
        };
        let Some(end) = part.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let Ok(headers) = std::str::from_utf8(&part[..end]) else {
            return false;
        };
        let mut disposition = None;
        for line in headers.split("\r\n") {
            let Some((key, value)) = line.split_once(':') else {
                return false;
            };
            if key.eq_ignore_ascii_case("content-disposition") {
                if disposition.replace(value.trim()).is_some() {
                    return false;
                }
            } else if !key.eq_ignore_ascii_case("content-type") {
                return false;
            }
        }
        let Some(disposition) = disposition else {
            return false;
        };
        let mut parameters = disposition.split(';');
        if parameters.next() != Some("form-data") {
            return false;
        }
        let mut name = None;
        let mut file = false;
        for parameter in parameters {
            let parameter = parameter.trim();
            if let Some(value) = parameter
                .strip_prefix("name=\"")
                .and_then(|s| s.strip_suffix('"'))
            {
                if name.replace(value).is_some() {
                    return false;
                }
            } else if parameter.starts_with("filename=\"") && parameter.ends_with('"') && !file {
                file = true;
            } else {
                return false;
            }
        }
        let Some(name) = name else {
            return false;
        };
        match name {
            "image" | "image[]" => {
                if !file {
                    return false;
                }
                image = true;
            }
            "mask" => {
                if !file {
                    return false;
                }
            }
            "prompt" | "model" | "n" | "size" | "quality" | "background" | "output_format"
            | "output_compression" | "moderation" | "user" => {
                if file {
                    return false;
                }
            }
            _ => return false,
        }
        let delimiter = format!("\r\n{marker}");
        let payload = &part[end + 4..];
        let Some(next) = payload
            .windows(delimiter.len())
            .position(|w| w == delimiter.as_bytes())
        else {
            return false;
        };
        if name == "model" {
            if model_seen {
                return false;
            }
            model_seen = true;
            *model = std::str::from_utf8(&payload[..next])
                .ok()
                .filter(|v| v.len() <= 256)
                .and_then(crate::observability::safe_identifier);
        }
        rest = &payload[next + delimiter.len()..];
    }
}

fn quota_evidence(headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(decoded) = hold::decoded_side(headers, body) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&decoded) else {
        return false;
    };
    [v.pointer("/error/code"), v.pointer("/error/type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|code| {
            matches!(
                code,
                "insufficient_quota"
                    | "image_quota_exceeded"
                    | "quota_exceeded"
                    | "billing_hard_limit_reached"
                    | "usage_limit_reached"
            )
        })
}

fn moderation(headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(decoded) = hold::decoded_side(headers, body) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&decoded) else {
        return false;
    };
    [v.pointer("/error/code"), v.pointer("/error/type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|code| {
            matches!(
                code,
                "content_policy_violation"
                    | "moderation_blocked"
                    | "moderation_rejected"
                    | "safety_error"
            )
        })
}

fn reset(headers: &HeaderMap, body: &[u8]) -> chrono::DateTime<chrono::Utc> {
    let now = chrono::Utc::now();
    // Honor actual absolute resets; do not cap known resets to an earlier time.
    let decoded = hold::decoded_side(headers, body);
    let v = decoded
        .as_deref()
        .and_then(|b| serde_json::from_slice::<Value>(b).ok());
    let absolute = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .or_else(|| v.as_ref()?.pointer("/error/resets_at")?.as_i64())
        .or_else(|| v.as_ref()?.pointer("/error/reset_at")?.as_i64())
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0));
    if let Some(at) = absolute.filter(|at| *at > now) {
        return at;
    }
    if let Some(seconds) = v
        .as_ref()
        .and_then(|v| v.pointer("/error/resets_in_seconds"))
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        && let Some(at) =
            chrono::Duration::try_seconds(seconds).and_then(|d| now.checked_add_signed(d))
    {
        return at;
    }
    if let Some(raw) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(seconds) = raw.parse::<i64>()
            && seconds > 0
            && let Some(at) =
                chrono::Duration::try_seconds(seconds).and_then(|d| now.checked_add_signed(d))
        {
            return at;
        }
        if let Ok(at) = chrono::DateTime::parse_from_rfc2822(raw)
            && at > now
        {
            return at.with_timezone(&chrono::Utc);
        }
    }
    // Missing/invalid reset: bounded one-hour image-only cooldown, no token work.
    now + chrono::Duration::hours(1)
}

fn request_model(headers: &HeaderMap, bytes: &[u8]) -> Option<String> {
    if headers.get_all("content-type").iter().count() != 1
        || !headers
            .get("content-type")?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .trim()
            .eq_ignore_ascii_case("application/json")
        || headers
            .get_all("content-encoding")
            .iter()
            .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return None;
    }
    // Bounded metadata parser skips prompts/images. No default model is invented
    // for incomplete, encoded or opaque bodies.
    wire_metadata::Metadata::parse(bytes)?.model()
}

async fn current_account(
    state: &RelayState,
    id: String,
) -> Result<Option<(Account, crate::accounts::Tokens)>, ()> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = store.transaction().map_err(|_| ())?;
        let account = store.read_account_unlocked(&id).map_err(|_| ())?;
        if !eligible(&account) {
            return Ok(None);
        }
        // Metadata and the current token generation come from one transaction.
        // An intervening refresh is observed, never requested by image traffic.
        let tokens = store.tokens_unlocked(&account).map_err(|_| ())?;
        Ok((!tokens.access_token.is_empty()).then_some((account, tokens)))
    })
    .await
    .map_err(|_| ())?
}

pub(super) async fn relay(state: Arc<RelayState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let edit = parts.uri.path().ends_with("/edits");
    let mut completion = Completion::new(
        state.observability.clone(),
        &parts.method,
        parts.uri.path(),
        Provider::Codex,
        &parts.headers,
    );
    completion.precommit_deadline = parts.extensions.get::<RequestDeadline>().map(|d| d.0);
    completion.track(state.inflight_requests.clone());
    completion.runtime = Some(state.accounts.clone());
    // Image traffic has no binding lease and never calls a responses selector.
    let session = session::classify(Provider::Codex, parts.uri.path(), &parts.headers, &[]);
    // current() prunes bindings; export is the existing read-only API. Codex
    // image sessions use their exact root key, never a guessed workspace label.
    let bound = session.key.as_deref().and_then(|key| {
        let snapshot = state.bindings.export().ok()?;
        snapshot
            .get("bindings")?
            .as_array()?
            .iter()
            .find(|b| b["key"].as_str() == Some(key) && b["accepted"].as_bool() != Some(false))?
            .get("account")?
            .as_str()
            .map(str::to_owned)
    });
    completion.session(&session);
    if state.memory_rejection(Provider::Codex).is_some() {
        return error(
            &mut completion,
            edit,
            StatusCode::SERVICE_UNAVAILABLE,
            "memory_guard",
        );
    }
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Edits require complete, bounded inspection before any account sees bytes.
    // Generations remain fully streaming. No reference ownership is guessed.
    let mut inspection_reservations = Vec::new();
    let mut model = None;
    let body = if edit {
        if parts.headers.get_all("content-type").iter().count() != 1 {
            return error(
                &mut completion,
                edit,
                StatusCode::BAD_REQUEST,
                "body_uninspectable",
            );
        }
        if parts
            .headers
            .get_all("content-encoding")
            .iter()
            .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return error(
                &mut completion,
                edit,
                StatusCode::BAD_REQUEST,
                "body_uninspectable",
            );
        }
        let mut stream = body.into_data_stream();
        let mut raw = Vec::new();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return error(
                    &mut completion,
                    edit,
                    StatusCode::BAD_REQUEST,
                    "body_uninspectable",
                );
            };
            if let Err(reason) = extend_edit(
                &mut raw,
                &mut inspection_reservations,
                &chunk,
                state.retain_cap,
                &state.budget,
            ) {
                let (status, code) = match reason {
                    EditAdmission::TooLarge => {
                        (StatusCode::PAYLOAD_TOO_LARGE, "body_uninspectable")
                    }
                    EditAdmission::Memory => (StatusCode::SERVICE_UNAVAILABLE, "memory_guard"),
                };
                return error(&mut completion, edit, status, code);
            }
        }
        let local = if content_type
            .split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        {
            local_json(&raw)
        } else if content_type.starts_with("multipart/form-data;") {
            local_multipart(&raw, content_type, &mut model)
        } else {
            false
        };
        if !local {
            return error(
                &mut completion,
                edit,
                StatusCode::CONFLICT,
                "reference_unknown",
            );
        }
        model = model.or_else(|| request_model(&parts.headers, &raw));
        Body::from(raw)
    } else {
        body
    };
    let upload = match upload::read(
        body,
        state.retain_cap,
        completion.input_bytes.clone(),
        state.budget.clone(),
        parts
            .headers
            .get("content-encoding")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"zstd")),
    )
    .await
    {
        Ok(u) => u,
        Err(_) => {
            return error(
                &mut completion,
                edit,
                StatusCode::BAD_REQUEST,
                "body_uninspectable",
            );
        }
    };
    completion.record.model = model.or_else(|| request_model(&parts.headers, &upload.prefix));
    let retained = upload.retained;
    completion.record.retained_copy = retained.is_some();
    let mut body = upload.body;
    let store = state.store.clone();
    let snapshots = tokio::task::spawn_blocking(move || {
        let metadata = AccountStore::list_metadata(store.home()).map_err(|_| ())?;
        let _gate = store.transaction().map_err(|_| ())?;
        let known: BTreeSet<_> = metadata.iter().map(|a| a.id.clone()).collect();
        Ok::<_, ()>((
            known,
            metadata
                .into_iter()
                .filter_map(|a| {
                    let a = store.read_account_unlocked(&a.id).ok()?;
                    if !eligible(&a) {
                        return None;
                    }
                    let tokens = store.tokens_unlocked(&a).ok()?;
                    (!tokens.access_token.is_empty()).then(|| (a, credential_key(&tokens)))
                })
                .collect::<Vec<_>>(),
        ))
    })
    .await;
    let Ok(Ok((known, mut candidates))) = snapshots else {
        return error(
            &mut completion,
            edit,
            StatusCode::BAD_GATEWAY,
            "transport_error",
        );
    };
    // Unset protects all Pro accounts conservatively; explicit false opts out.
    // Last-resort priority is stronger than affinity, without account-name rules.
    // The bound account is preferred among accounts in the same priority tier.
    candidates.sort_by_key(|(a, _)| {
        (
            state
                .image_account_config
                .get(&a.id)
                .and_then(|c| c.images_last_resort)
                .unwrap_or(a.plan.as_deref() == Some("pro")),
            bound.as_deref() != Some(a.id.as_str()),
            a.id.clone(),
        )
    });
    let mut earliest = None;
    let mut incapable = false;
    {
        let mut health = state.images.lock().unwrap_or_else(|e| e.into_inner());
        health.accounts.retain(|id, _| known.contains(id));
        candidates.retain(|(a, credential)| {
            let Some(h) = health.accounts.get(&a.id) else {
                return true;
            };
            if h.incapable && h.credential == *credential {
                incapable = true;
                return false;
            }
            if let Some(at) = h.exhausted.filter(|at| *at > chrono::Utc::now()) {
                earliest =
                    Some(earliest.map_or(at, |old: chrono::DateTime<chrono::Utc>| old.min(at)));
                return false;
            }
            true
        });
    }
    if candidates.is_empty() {
        let reason = if earliest.is_some() {
            "quota_exhausted"
        } else if incapable {
            "auth_rejected"
        } else if bound.is_some() {
            "bound_account_unavailable"
        } else {
            "no_eligible_account"
        };
        let status = if earliest.is_some() {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::BAD_REQUEST
        };
        let mut response = error(&mut completion, edit, status, reason);
        if let Some(at) = earliest {
            retry_after(response.headers_mut(), at);
        }
        return response;
    }
    let count = candidates.len();
    let mut pending_quota = None;
    for (index, (snapshot, _)) in candidates.into_iter().enumerate() {
        // No await occurs between this transaction result and request dispatch.
        // Reload settings, health and tokens after every prior quota response.
        let (account, tokens) = match current_account(&state, snapshot.id).await {
            Ok(Some(current)) => current,
            Ok(None) | Err(()) => continue,
        };
        {
            let mut health = state.images.lock().unwrap_or_else(|e| e.into_inner());
            let h = health.observe(&account.id, account.generation, credential_key(&tokens));
            if (h.incapable && h.credential == credential_key(&tokens))
                || h.exhausted.is_some_and(|at| at > chrono::Utc::now())
            {
                continue;
            }
        }
        completion.record.account = Some(account.id.clone());
        completion.attempt(&account.id);
        let upstream_headers =
            match headers::upstream(&parts.headers, &account, &tokens.access_token) {
                Ok(h) => h,
                Err(_) => {
                    return error(
                        &mut completion,
                        edit,
                        StatusCode::BAD_GATEWAY,
                        "auth_rejected",
                    );
                }
            };
        let mut url = format!(
            "{}{}",
            state.codex_base,
            parts.uri.path().strip_prefix("/v1").unwrap()
        );
        if let Some(query) = parts.uri.query() {
            url.push('?');
            url.push_str(query);
        }
        let upstream = match state
            .client
            .post(url)
            .headers(upstream_headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => {
                return error(
                    &mut completion,
                    edit,
                    StatusCode::BAD_GATEWAY,
                    "transport_error",
                );
            }
        };
        let status = upstream.status();
        let mut response_headers = headers::filtered(upstream.headers(), false);
        let original_headers = upstream.headers().clone();
        let held = hold::hold(
            status,
            &original_headers,
            Box::pin(upstream.bytes_stream()),
            Duration::from_secs(2),
        )
        .await;
        // Incomplete/opaque errors cannot establish absence of moderation.
        // Preserve their bytes and never replay the prompt across accounts.
        let quota_code = quota_evidence(&response_headers, &held.prefix);
        let classified = (held.complete || quota_code)
            && held.terminal.is_none()
            && hold::decoded_side(&original_headers, &held.prefix).is_some_and(|bytes| {
                bytes.is_empty() || serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_ok()
            });
        let is_moderation = moderation(&response_headers, &held.prefix);
        let quota =
            classified && !is_moderation && (status == StatusCode::TOO_MANY_REQUESTS || quota_code);
        let auth = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
        let reason = if !classified || is_moderation {
            None
        } else if auth {
            Some("auth_rejected")
        } else if quota {
            Some("quota_exhausted")
        } else if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            Some("endpoint_unsupported")
        } else {
            None
        };
        if auth || quota {
            let at = quota.then(|| reset(&response_headers, &held.prefix));
            state
                .images
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failure(
                    &account.id,
                    account.generation,
                    credential_key(&tokens),
                    auth,
                    at,
                );
            if let Some(at) = at {
                retry_after(&mut response_headers, at);
                // Only a quota response permits failover, and only a completely
                // retained upload can be replayed. Transport/auth never retry.
                if !auth
                    && index + 1 < count
                    && let Some(handle) = &retained
                    && let Ok(Some(replay)) =
                        tokio::time::timeout(Duration::from_millis(100), handle.retry_body()).await
                {
                    completion.failover("image_quota");
                    completion.rate_retry();
                    completion.end_attempt();
                    body = replay;
                    pending_quota = Some(UpstreamReply {
                        account,
                        status,
                        headers: response_headers,
                        held,
                        reason,
                    });
                    continue;
                }
            }
        }
        return upstream_response(
            edit,
            parts.extensions.get::<CommitSignal>().cloned(),
            completion,
            retained,
            inspection_reservations,
            UpstreamReply {
                account,
                status,
                headers: response_headers,
                held,
                reason,
            },
        );
    }
    if let Some(reply) = pending_quota {
        return upstream_response(
            edit,
            parts.extensions.get::<CommitSignal>().cloned(),
            completion,
            retained,
            inspection_reservations,
            reply,
        );
    }
    error(
        &mut completion,
        edit,
        StatusCode::CONFLICT,
        "account_unavailable",
    )
}

struct UpstreamReply {
    account: Account,
    status: StatusCode,
    headers: HeaderMap,
    held: hold::HeldResponse,
    reason: Option<&'static str>,
}

fn upstream_response(
    edit: bool,
    commit: Option<CommitSignal>,
    mut completion: Completion,
    retained: Option<upload::RetainedHandle>,
    inspection_reservations: Vec<crate::guard::Reservation>,
    reply: UpstreamReply,
) -> Response {
    let UpstreamReply {
        account,
        status,
        headers: mut response_headers,
        held,
        reason,
    } = reply;
    let upstream_status = status;
    // Classified failures must reach Codex's tool through the response body.
    // Moderation/unclassified failures retain their original stream and bytes.
    let (status, held) = if let Some(reason) = reason {
        let status = if reason == "quota_exhausted" {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            status
        };
        let prefix = Bytes::from(
            serde_json::to_vec(&error_body(edit, reason)).expect("fixed image error JSON"),
        );
        for header in [
            "content-length",
            "content-encoding",
            "content-range",
            "etag",
            "content-md5",
            "digest",
            "content-digest",
            "repr-digest",
            "last-modified",
        ] {
            response_headers.remove(header);
        }
        response_headers.insert("content-type", HeaderValue::from_static("application/json"));
        response_headers.insert("cache-control", HeaderValue::from_static("no-store"));
        (
            status,
            hold::HeldResponse {
                prefix,
                remainder: None,
                stream: Box::pin(futures_util::stream::empty()),
                terminal: None,
                decision: hold::CommitDecision::Status,
                complete: true,
            },
        )
    } else {
        (status, held)
    };
    if let Ok(label) = HeaderValue::from_str(&account.label) {
        response_headers.insert("x-qb-image-account", label);
    }
    if let Some(reason) = reason {
        let code = format!(
            "image_{}_{reason}",
            if edit { "edit" } else { "generation" }
        );
        response_headers.insert("x-qb-image-error", HeaderValue::from_str(&code).unwrap());
    }
    let final_outcome = match reason {
        Some(reason) => format!(
            "image_{}_{reason}",
            if edit { "edit" } else { "generation" }
        ),
        None if status.is_success() => "ok".into(),
        None => format!(
            "image_{}_upstream_error",
            if edit { "edit" } else { "generation" }
        ),
    };
    if reason.is_some() {
        completion.record.upstream_error_code = Some(final_outcome.clone());
    }
    completion.outcome(status.as_u16(), &final_outcome);
    completion.record.status = status.as_u16();
    completion.record.upstream_status = Some(upstream_status.as_u16());
    completion.record.ttfb_ms = completion.elapsed_ms();
    if let Some(signal) = commit {
        signal.0.send_replace(true);
    }
    let stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
    > = Box::pin(async_stream::try_stream! {
        // Keep retention/budget and ledger alive until consumption or drop.
        let _retained = retained;
        let _inspection = inspection_reservations;
        let mut completion = completion;
        let hold::HeldResponse { prefix, remainder, mut stream, terminal, .. } = held;
        for bytes in std::iter::once(prefix).chain(remainder) {
            completion.record.bytes_out += bytes.len() as u64;
            yield bytes;
        }
        if let Some(error) = terminal {
            completion.outcome(status.as_u16(), "upstream_stream_error");
            Err(error)?;
        }
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    completion.outcome(status.as_u16(), "upstream_stream_error");
                    Err(std::io::Error::other("이미지 응답 스트림이 중단되었습니다."))?
                }
            };
            completion.record.bytes_out += chunk.len() as u64;
            yield chunk;
        }
        completion.finish(&final_outcome);
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

fn retry_after(headers: &mut HeaderMap, at: chrono::DateTime<chrono::Utc>) {
    if !headers.contains_key("retry-after") {
        let seconds = (at - chrono::Utc::now()).num_seconds().max(1);
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&seconds.to_string()).unwrap(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_edit_admission_has_logarithmic_records_and_returns_all_budget() {
        let cap = 1024 * 1024;
        let budget = crate::guard::InflightBudget::new(cap * 5);
        let mut raw = Vec::new();
        let mut reservations = Vec::new();
        for _ in 0..cap {
            assert!(extend_edit(&mut raw, &mut reservations, b"x", cap, &budget).is_ok());
            assert!(extend_edit(&mut raw, &mut reservations, b"", cap, &budget).is_ok());
        }
        assert_eq!(raw.len(), cap);
        assert_eq!(reservations.len(), 9);
        assert_eq!(budget.used(), cap * 4 + 128 * 9);
        assert!(matches!(
            extend_edit(&mut raw, &mut reservations, b"x", cap, &budget),
            Err(EditAdmission::TooLarge)
        ));
        drop(raw);
        drop(reservations);
        assert_eq!(budget.used(), 0);
        let small = crate::guard::InflightBudget::new(4096 * 4);
        assert!(matches!(
            extend_edit(&mut Vec::new(), &mut Vec::new(), b"x", cap, &small),
            Err(EditAdmission::Memory)
        ));
        assert_eq!(small.used(), 0);
    }

    #[test]
    fn concurrent_restrictions_merge_and_old_credential_failures_cannot_replace_newer_evidence() {
        let mut state = ImageState::default();
        let soon = chrono::Utc::now() + chrono::Duration::seconds(1);
        let later = soon + chrono::Duration::hours(1);
        state.failure("a", 1, [1; 32], true, None);
        state.failure("a", 1, [1; 32], false, Some(later));
        state.failure("a", 1, [1; 32], false, Some(soon));
        let h = state.observe("a", 2, [1; 32]);
        assert!(
            h.incapable,
            "metadata changes do not pardon the same credential"
        );
        assert_eq!(h.exhausted, Some(later));
        assert!(!state.observe("a", 3, [2; 32]).incapable);
        state.failure("a", 1, [1; 32], true, None);
        let h = state.observe("a", 3, [2; 32]);
        assert!(
            !h.incapable,
            "a late old-token rejection cannot poison the replacement"
        );
        assert_eq!(
            h.exhausted,
            Some(later),
            "quota is account-wide across credentials"
        );
        state.failure("a", 3, [2; 32], true, None);
        state.failure("a", 1, [1; 32], false, Some(soon));
        assert!(state.observe("a", 3, [2; 32]).incapable);
    }

    #[test]
    fn model_receipts_use_actual_metadata_without_opaque_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        assert_eq!(
            request_model(&headers, br#"{"model":"gpt-image-2","prompt":"private"}"#).as_deref(),
            Some("gpt-image-2")
        );
        assert!(request_model(&headers, br#"{"prompt":"gpt-image-2"}"#).is_none());
        assert!(request_model(&headers, br#"{"model":"gpt-image-2""#).is_none());
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        assert!(request_model(&headers, br#"{"model":"gpt-image-2"}"#).is_none());
        let raw = b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-2\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"local.png\"\r\n\r\nlocalbytes\r\n--b--\r\n";
        let mut model = None;
        assert!(local_multipart(
            raw,
            "multipart/form-data; boundary=b",
            &mut model
        ));
        assert_eq!(model.as_deref(), Some("gpt-image-2"));
    }

    #[test]
    fn actual_quota_reset_and_bounded_missing_reset() {
        let now = chrono::Utc::now();
        let at = (now + chrono::Duration::days(3)).timestamp();
        let body = format!("{{\"error\":{{\"resets_at\":{at}}}}}");
        assert_eq!(reset(&HeaderMap::new(), body.as_bytes()).timestamp(), at);
        let headers = HeaderMap::new();
        let fallback = reset(&headers, b"{}");
        assert!(fallback >= now + chrono::Duration::minutes(59));
        assert!(fallback <= chrono::Utc::now() + chrono::Duration::hours(1));
        let at = reset(&headers, br#"{"error":{"resets_in_seconds":7200}}"#);
        assert!(at >= now + chrono::Duration::seconds(7200));
    }

    #[test]
    fn multipart_disposition_ambiguity_is_not_an_ownership_proof() {
        for disposition in [
            "name=\"image\"; name=\"file_id\"; filename=\"local.png\"",
            "filename=\"image\"; name=\"file_id\"",
            "name=\"image\"",
        ] {
            let body = format!(
                "--b\r\nContent-Disposition: form-data; {disposition}\r\n\r\nfile-server\r\n--b--\r\n"
            );
            assert!(!local_multipart(
                body.as_bytes(),
                "multipart/form-data; boundary=b",
                &mut None
            ));
        }
    }
}
