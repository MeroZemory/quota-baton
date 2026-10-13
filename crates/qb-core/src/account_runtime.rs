use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, Weak},
};

use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, UserState},
    observability::Observability,
};

#[derive(Clone, Debug)]
pub enum HealthSignal {
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: String,
    },
    Revoked(String),
    AuthFailure,
    Ready,
}

#[derive(Clone)]
struct LimitResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

pub struct AccountRuntime {
    pub store: Arc<AccountStore>,
    obs: Arc<Observability>,
    events: broadcast::Sender<Value>,
    limits: Mutex<BTreeMap<String, LimitResponse>>,
    allowance: f64,
    probes: Mutex<BTreeSet<String>>,
    volatile: Mutex<BTreeMap<String, Health>>,
    inflight: Mutex<BTreeMap<String, usize>>,
}

pub struct InflightGuard {
    runtime: Weak<AccountRuntime>,
    id: String,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut counts) = runtime.inflight.lock()
            && let Some(count) = counts.get_mut(&self.id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.id);
            }
        }
    }
}

pub struct Probe {
    runtime: Weak<AccountRuntime>,
    id: String,
}
#[derive(Debug, thiserror::Error)]
#[error("account authentication probe is already running")]
pub struct ProbeBusy;
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut probes) = runtime.probes.lock()
        {
            probes.remove(&self.id);
        }
    }
}

impl AccountRuntime {
    pub fn new(store: Arc<AccountStore>, obs: Arc<Observability>, allowance: f64) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let writer = obs.clone();
        let changes = events.clone();
        store.set_observer(Arc::new(move |account,previous| {
            if previous != &account.health {writer.record_health(&account.id,previous,&account.health);}
            let _=changes.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
        }));
        Arc::new(Self {
            store,
            obs,
            events,
            limits: Mutex::new(BTreeMap::new()),
            allowance,
            probes: Mutex::new(BTreeSet::new()),
            volatile: Mutex::new(BTreeMap::new()),
            inflight: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn track_inflight(self: &Arc<Self>, id: &str) -> InflightGuard {
        if let Ok(mut counts) = self.inflight.lock() {
            let count = counts.entry(id.to_owned()).or_default();
            *count = count.saturating_add(1);
        }
        InflightGuard {
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
        }
    }

    pub fn inflight_count(&self, id: &str) -> usize {
        self.inflight
            .lock()
            .ok()
            .and_then(|counts| counts.get(id).copied())
            .unwrap_or(0)
    }

    /// Placement reservations precede the first upstream attempt. Apply the C5
    /// limit reducer under the store transaction so the 2B fallback cannot spend
    /// credits before an upstream response reports the additional in-flight work.
    pub(crate) fn projected_quota(
        &self,
        id: &str,
        bindings: &crate::bindings::BindingTable,
        model: Option<&str>,
    ) -> Result<Account, crate::accounts::StoreError> {
        self.store.update_metadata(id, |account| {
            if !account.eligible(model) {
                return;
            }
            let now = Utc::now();
            let Some(mut snapshot) = self.obs.quota_snapshot(id) else {
                return;
            };
            let count = bindings.pending_count(id);
            let traffic = bindings.has_traffic(id);
            let Some(windows) = snapshot["windows"].as_array_mut() else {
                return;
            };
            if traffic {
                windows.retain(|window| {
                    window["observed_at"]
                        .as_str()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .is_some_and(|observed| {
                            (now - observed.with_timezone(&Utc)).num_seconds() <= 900
                        })
                });
            }
            if windows.is_empty() {
                return;
            }
            // This is a projection, not a fresh observation of rejection flags.
            snapshot["signals"] = Value::Null;
            let mut projected = account.clone();
            projected.health = Health::Ready;
            projected.quota_cooling = None;
            projected.apply_health_signal(
                crate::accounts::HealthSignal::UsageSnapshot {
                    snapshot,
                    model: model.map(str::to_owned),
                    projected_allowance: self.allowance * count as f64,
                },
                now,
            );
            if let Health::Cooling {
                reason: HealthReason::Quota,
                until,
                scope,
            } = projected.health
            {
                account.apply_health_signal(
                    crate::accounts::HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        scope,
                    },
                    now,
                );
                account.quota_cooling = projected.quota_cooling;
            }
        })
    }

    pub fn probing(&self, id: &str) -> bool {
        self.probes.lock().map(|p| p.contains(id)).unwrap_or(true)
    }
    pub fn blocked_in_memory(&self, id: &str) -> bool {
        self.volatile
            .lock()
            .map(|v| v.contains_key(id))
            .unwrap_or(true)
    }
    pub fn clear_memory_health(&self, account: &Account) {
        if let Ok(mut health) = self.volatile.lock() {
            health.remove(&account.id);
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }
    pub fn begin_probe(self: &Arc<Self>, account: &Account) -> Result<Option<Probe>, ProbeBusy> {
        if (!account.auth_probe_pending && account.auth_failures == 0)
            || account.health != Health::Ready
        {
            return Ok(None);
        }
        let mut probes = self.probes.lock().map_err(|_| ProbeBusy)?;
        if !probes.insert(account.id.clone()) {
            return Err(ProbeBusy);
        }
        Ok(Some(Probe {
            runtime: Arc::downgrade(self),
            id: account.id.clone(),
        }))
    }
    pub async fn probe_success(&self, id: &str) -> Result<(), ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            store
                .update_metadata(&id, |a| {
                    if a.health == Health::Ready {
                        a.auth_failures = 0;
                        a.auth_probe_pending = false;
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        Ok(())
    }

    pub fn emit_account(&self, account: &Account) {
        if let Ok(mut health) = self.volatile.lock() {
            health.insert(account.id.clone(), account.health.clone());
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }

    pub async fn transition(&self, id: &str, signal: HealthSignal) -> Result<Account, ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || {
            let updated = store
                .update_metadata(&id, |account| {
                    use crate::accounts::HealthSignal as Signal;
                    let signal = match signal {
                        HealthSignal::Cooling {
                            reason,
                            until,
                            scope,
                        } => Signal::Cooling {
                            reason,
                            until,
                            scope,
                        },
                        HealthSignal::Revoked(reason) => Signal::Revoked { reason },
                        HealthSignal::AuthFailure => Signal::AuthAfterRefresh,
                        HealthSignal::Ready => Signal::Ready,
                    };
                    account.apply_health_signal(signal, Utc::now());
                })
                .map_err(|_| ())?;
            Ok::<_, ()>(updated)
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn eligible(&self, id: &str, model: Option<&str>) -> bool {
        if self.blocked_in_memory(id) || self.probing(id) {
            return false;
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let model = model.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            store.load(&id).is_ok_and(|a| a.eligible(model.as_deref()))
        })
        .await
        .unwrap_or(false)
    }

    pub async fn change(
        &self,
        id: &str,
        user_state: Option<UserState>,
        label: Option<String>,
        drain: Option<bool>,
        allow_credits: Option<bool>,
    ) -> Result<Account, ()> {
        if label
            .as_ref()
            .is_some_and(|l| l.len() > 256 || l.chars().any(char::is_control))
        {
            return Err(());
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let credit_release = allow_credits == Some(true)
            && self.obs.quota_snapshot(&id).is_some_and(|q| {
                q["windows"].as_array().is_some_and(|w| {
                    !w.is_empty()
                        && w.iter()
                            .all(|w| w["used_pct"].as_f64().is_some_and(|p| p < 100.0))
                        && w.iter().any(|w| {
                            w["used_pct"].as_f64().is_some_and(|p| {
                                p + self.allowance * self.inflight_count(&id) as f64 >= 99.0
                            })
                        })
                }) && q["signals"]["limit_reached"] != true
                    && q["signals"]["unified_status"] != "rejected"
            });
        let account = tokio::task::spawn_blocking(move || {
            store
                .update_settings(&id, drain, allow_credits, |a| {
                    if let Some(state) = user_state {
                        a.user_state = state;
                    }
                    if let Some(label) = label {
                        a.label = label;
                    }
                    if let Some(drain) = drain {
                        a.drain = drain;
                    }
                    if let Some(allow) = allow_credits {
                        a.allow_credits = allow;
                    }
                    if credit_release
                        && matches!(
                            a.health,
                            Health::Cooling {
                                reason: HealthReason::Quota,
                                ..
                            }
                        )
                    {
                        a.apply_health_signal(crate::accounts::HealthSignal::Ready, Utc::now());
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn initialize_settings(
        &self,
        settings: std::collections::BTreeMap<String, crate::config::AccountConfig>,
    ) -> Result<(), ()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            for account in store.list().map_err(|_| ())? {
                if let Some(settings) = settings.get(&account.id)
                    && (account.drain != settings.drain
                        || account.allow_credits != settings.allow_credits)
                {
                    store
                        .update_metadata(&account.id, |a| {
                            a.drain = settings.drain;
                            a.allow_credits = settings.allow_credits;
                        })
                        .map_err(|_| ())?;
                }
            }
            Ok::<_, ()>(())
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn views(&self) -> Result<Vec<Value>, ()> {
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list().map_err(|_| ()))
            .await
            .map_err(|_| ())??;
        Ok(accounts.into_iter().map(|a| {
            let quota = self.obs.quota_snapshot(&a.id).unwrap_or_default();
            let health=self.volatile.lock().ok().and_then(|v|v.get(&a.id).cloned()).unwrap_or(a.health);
            let pending = self.probing(&a.id) || ((a.auth_probe_pending || a.auth_failures > 0) && health == Health::Ready);
            let health = if pending { json!({"kind":"cooling","reason":"auth","until":Utc::now(),"scope":"account","probe_pending":true}) } else { json!(health) };
            json!({"id":a.id,"provider":a.provider,"label":a.label,"email":a.email,"plan":quota["plan"].as_str().map(str::to_owned).or(a.plan),"user_state":a.user_state,"health":health,"probe_pending":pending,"windows":quota["windows"].as_array().cloned().unwrap_or_default(),"credits":quota["credits"],"last_used_at":null,"bound_sessions":0,"token_expires_at":a.token_meta.expires_at,"drain":a.drain,"allow_credits":a.allow_credits})
        }).collect())
    }

    pub async fn observe_headers(&self, id: &str, provider: Provider, headers: &HeaderMap) {
        self.observe_headers_for_model(id, provider, headers, None)
            .await;
    }

    pub async fn observe_headers_for_model(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
        model: Option<&str>,
    ) {
        let previous = self.obs.quota_snapshot(id);
        self.obs.observe_headers(id, provider, headers);
        if previous != self.obs.quota_snapshot(id)
            && let Some(snapshot) = self.obs.quota_snapshot(id)
        {
            let _ = self.apply_quota_snapshot(id, snapshot, model).await;
        }
    }

    pub async fn observe_codex_event(&self, id: &str, event: &Value) {
        self.observe_codex_event_for_model(id, event, None).await;
    }

    pub async fn observe_codex_event_for_model(
        &self,
        id: &str,
        event: &Value,
        model: Option<&str>,
    ) {
        if event["type"] != "codex.rate_limits" {
            return;
        }
        self.obs.observe_codex_event(id, event);
        if let Some(snapshot) = self.obs.quota_snapshot(id) {
            let _ = self.apply_quota_snapshot(id, snapshot, model).await;
        }
    }

    /// Apply an observed snapshot through the same persisted reducer as outcomes
    /// and credential failures. Per-window timestamps identify fresh evidence.
    pub async fn apply_quota_snapshot(
        &self,
        id: &str,
        snapshot: Value,
        model: Option<&str>,
    ) -> Result<Account, ()> {
        let store = self.store.clone();
        let account_id = id.to_owned();
        let model = model.map(str::to_owned);
        let projected_allowance = self.allowance * self.inflight_count(id) as f64;
        let observation = snapshot.clone();
        let account = tokio::task::spawn_blocking(move || {
            store
                .update_metadata(&account_id, |account| {
                    account.apply_health_signal(
                        crate::accounts::HealthSignal::UsageSnapshot {
                            snapshot: observation,
                            model,
                            projected_allowance,
                        },
                        Utc::now(),
                    );
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        let _ = self
            .events
            .send(json!({"event":"quota_updated","data":{"id":id,"snapshot":snapshot}}));
        Ok(account)
    }

    pub fn cache_limit(&self, id: &str, status: StatusCode, headers: &HeaderMap, raw: &[u8]) {
        if raw.len() > 65536 {
            return;
        }
        let headers = headers
            .iter()
            .filter(|(name, _)| {
                let n = name.as_str();
                n.starts_with("x-codex-")
                    || n.starts_with("anthropic-ratelimit-")
                    || matches!(n, "retry-after" | "content-type" | "content-encoding")
            })
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        if let Ok(mut limits) = self.limits.lock() {
            limits.insert(
                id.into(),
                LimitResponse {
                    status,
                    headers,
                    body: Bytes::copy_from_slice(raw),
                },
            );
        }
    }

    pub async fn unavailable(
        &self,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        // M5는 여기서 후보별 최소 간격을 지키는 사용량 poll을 실행한다.
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let candidates: Vec<_> = accounts
            .iter()
            .filter(|a| a.provider == provider && a.user_state == UserState::Active)
            .collect();
        let volatile = self.volatile.lock().map(|v| v.clone()).unwrap_or_default();
        let now = Utc::now();
        let mut cooling = Vec::new();
        for a in &candidates {
            for health in std::iter::once(&a.health).chain(volatile.get(&a.id)) {
                if let Health::Cooling {
                    reason,
                    until,
                    scope,
                } = health
                    && *until > now
                    && (scope == "account" || model.is_some_and(|m| scope == &format!("model:{m}")))
                {
                    let replay = matches!(
                        reason,
                        HealthReason::Quota | HealthReason::Rate | HealthReason::Auth
                    );
                    cooling.push((*a, *until, replay));
                }
            }
            if self.probing(&a.id)
                || ((a.auth_probe_pending || a.auth_failures > 0) && a.health == Health::Ready)
            {
                cooling.push((*a, now + Duration::seconds(1), false));
            }
        }
        if !cooling.is_empty() {
            // Prefer genuine limit-response resets when available. Other
            // temporary blockers still produce row 14, using the native shape.
            cooling.sort_by_key(|(_, until, replay)| (!*replay, *until));
            let (a, until, replay) = cooling[0];
            let seconds = (until - Utc::now()).num_seconds().max(1);
            let mut result = if !websocket
                && replay
                && let Some(cache) = self.limits.lock().ok().and_then(|m| m.get(&a.id).cloned())
            {
                let mut r = Response::new(Body::from(cache.body));
                *r.status_mut() = cache.status;
                *r.headers_mut() = cache.headers;
                r
            } else {
                let error = if provider == Provider::Codex {
                    json!({"error":{"type":"usage_limit_reached","message":"quota-baton: all accounts are cooling","resets_at":until.timestamp(),"resets_in_seconds":seconds}})
                } else {
                    json!({"type":"error","error":{"type":"rate_limit_error","message":"quota-baton: all accounts are cooling"}})
                };
                (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
            };
            result.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&seconds.to_string()).unwrap(),
            );
            result
        } else {
            let mut error = json!({"error":{"type":"invalid_request_error","message":format!("quota-baton: no usable {} account — open the tray",provider.as_str())}});
            if provider == Provider::Claude {
                error["type"] = "error".into();
            }
            (StatusCode::BAD_REQUEST, Json(error)).into_response()
        }
    }
}
