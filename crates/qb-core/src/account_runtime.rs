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
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
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
        if account.auth_failures == 0 || account.health != Health::Ready {
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
                    account.health = match signal {
                        HealthSignal::Cooling {
                            reason,
                            until,
                            scope,
                        } if !matches!(account.health, Health::NeedsRelogin { .. }) => {
                            Health::Cooling {
                                reason,
                                until,
                                scope,
                            }
                        }
                        HealthSignal::Cooling { .. } => account.health.clone(),
                        HealthSignal::Revoked(reason) => Health::NeedsRelogin {
                            reason,
                            since: Utc::now(),
                        },
                        HealthSignal::AuthFailure
                            if !matches!(account.health, Health::NeedsRelogin { .. }) =>
                        {
                            let minutes =
                                (15_i64 * (1_i64 << account.auth_failures.min(4))).min(240);
                            account.auth_failures = account.auth_failures.saturating_add(1);
                            Health::Cooling {
                                reason: HealthReason::Auth,
                                until: Utc::now() + Duration::minutes(minutes),
                                scope: "account".into(),
                            }
                        }
                        HealthSignal::AuthFailure => account.health.clone(),
                        HealthSignal::Ready
                            if !matches!(account.health, Health::NeedsRelogin { .. }) =>
                        {
                            Health::Ready
                        }
                        HealthSignal::Ready => account.health.clone(),
                    };
                })
                .map_err(|_| ())?;
            Ok::<_, ()>(updated)
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn eligible(&self, id: &str, model: Option<&str>) -> bool {
        if self.blocked_in_memory(id) {
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
                            w["used_pct"]
                                .as_f64()
                                .is_some_and(|p| p + self.allowance >= 99.0)
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
                        a.health = Health::Ready;
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
            json!({"id":a.id,"provider":a.provider,"label":a.label,"email":a.email,"plan":quota["plan"].as_str().map(str::to_owned).or(a.plan),"user_state":a.user_state,"health":health,"windows":quota["windows"].as_array().cloned().unwrap_or_default(),"credits":quota["credits"],"last_used_at":null,"bound_sessions":0,"token_expires_at":a.token_meta.expires_at,"drain":a.drain,"allow_credits":a.allow_credits})
        }).collect())
    }

    pub async fn observe_headers(&self, id: &str, provider: Provider, headers: &HeaderMap) {
        let previous = self.obs.quota_snapshot(id);
        self.obs.observe_headers(id, provider, headers);
        if previous != self.obs.quota_snapshot(id) {
            self.apply_snapshot(id, provider).await;
        }
    }

    pub async fn observe_codex_event(&self, id: &str, event: &Value) {
        if event["type"] != "codex.rate_limits" {
            return;
        }
        self.obs.observe_codex_event(id, event);
        self.apply_snapshot(id, Provider::Codex).await;
    }

    async fn apply_snapshot(&self, id: &str, provider: Provider) {
        let Some(snapshot) = self.obs.quota_snapshot(id) else {
            return;
        };
        let store = self.store.clone();
        let account_id = id.to_owned();
        let Ok(Ok(account)) = tokio::task::spawn_blocking(move || store.load(&account_id)).await
        else {
            return;
        };
        let windows = snapshot["windows"].as_array().cloned().unwrap_or_default();
        let reset = |w: &Value| {
            w["reset_at"]
                .as_str()
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|v| v.with_timezone(&Utc))
        };
        let depleted: Vec<_> = windows
            .iter()
            .filter(|w| {
                w["used_pct"].as_f64().is_some_and(|p| p >= 100.0) || w["status"] == "rejected"
            })
            .collect();
        let credits_hold = provider == Provider::Codex
            && !account.allow_credits
            && (windows.iter().any(|w| {
                w["used_pct"]
                    .as_f64()
                    .is_some_and(|p| p + self.allowance >= 99.0)
            }) || snapshot["signals"]["serving_from_credits"] == true);
        let exhausted = !depleted.is_empty()
            || snapshot["signals"]["limit_reached"] == true
            || snapshot["signals"]["unified_status"] == "rejected"
            || credits_hold;
        let until = depleted
            .iter()
            .filter_map(|w| reset(w))
            .max()
            .or_else(|| windows.iter().filter_map(reset).max())
            .unwrap_or_else(|| Utc::now() + Duration::minutes(5));
        if exhausted
            && matches!(
                account.health,
                Health::Ready
                    | Health::Cooling {
                        reason: HealthReason::Quota,
                        ..
                    }
            )
        {
            let _ = self
                .transition(
                    id,
                    HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        scope: "account".into(),
                    },
                )
                .await;
        } else if let Health::Cooling {
            reason: HealthReason::Quota,
            until: old,
            ..
        } = account.health
            && !windows.is_empty()
            && windows
                .iter()
                .all(|w| w["used_pct"].as_f64().is_some_and(|p| p < 100.0))
            && windows.iter().filter_map(reset).any(|new| new != old)
            && !exhausted
        {
            let _ = self.transition(id, HealthSignal::Ready).await;
        }
        let _ = self
            .events
            .send(json!({"event":"quota_updated","data":{"id":id,"snapshot":snapshot}}));
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
        let mut cooling = Vec::new();
        for a in &candidates {
            if let Health::Cooling {
                reason: HealthReason::Quota | HealthReason::Rate | HealthReason::Auth,
                until,
                scope,
            } = &a.health
                && (scope == "account" || model.is_none_or(|m| scope == &format!("model:{m}")))
            {
                cooling.push((*a, *until));
            }
        }
        if !candidates.is_empty() && cooling.len() == candidates.len() {
            cooling.sort_by_key(|(_, until)| *until);
            let (a, until) = cooling[0];
            let seconds = (until - Utc::now()).num_seconds().max(1);
            let mut result = if !websocket
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
            let mut error = json!({"error":{"type":"invalid_request_error","message":format!("quota-baton: no usable {} account - open the quota-baton tray (사용 가능한 계정이 없습니다. 트레이를 여세요)",provider.as_str())}});
            if provider == Provider::Claude {
                error["type"] = "error".into();
            }
            (StatusCode::BAD_REQUEST, Json(error)).into_response()
        }
    }
}
