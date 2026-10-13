use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

use crate::{
    accounts::{Account, AccountStore, ImportMode, Source, StoreError, Tokens, linked},
    observability::Observability,
};

#[derive(Default)]
pub(super) struct TokenGates(Mutex<BTreeMap<String, Arc<Mutex<ReloadClock>>>>);

#[derive(Default)]
struct ReloadClock {
    before_expiry: Option<Instant>,
    stale_event: Option<Instant>,
}
impl ReloadClock {
    fn admit(last: &mut Option<Instant>, now: Instant, interval: Duration) -> bool {
        if last.is_some_and(|last| now.duration_since(last) < interval) {
            return false;
        }
        *last = Some(now);
        true
    }
    fn before_expiry(&mut self) -> bool {
        Self::admit(
            &mut self.before_expiry,
            Instant::now(),
            Duration::from_secs(60),
        )
    }
    fn stale_event(&mut self) -> bool {
        Self::admit(
            &mut self.stale_event,
            Instant::now(),
            Duration::from_secs(600),
        )
    }
}

impl TokenGates {
    async fn gate(&self, id: &str) -> Arc<Mutex<ReloadClock>> {
        self.0
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub async fn before_request(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        id: String,
    ) -> Result<(Account, Tokens), ()> {
        let gate = self.gate(&id).await;
        // blocking 작업이 요청 취소 후에도 계속되므로 lock 소유권을 작업 안으로 옮긴다.
        let guard = gate.lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut clock = guard;
            let (mut account, _) = snapshot(&store, &id)?;
            if is_linked(&account)
                && account.token_meta.expires_at.is_some_and(|expiry| {
                    expiry <= chrono::Utc::now() + chrono::Duration::minutes(10)
                })
                && clock.before_expiry()
            {
                account = reload(&store, &observability, account, "before_expiry", &mut clock)?;
            }
            snapshot(&store, &account.id)
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn after_401(
        &self,
        store: Arc<AccountStore>,
        observability: Arc<Observability>,
        account: Account,
        failed: &Tokens,
    ) -> Result<Option<(Account, Tokens)>, ()> {
        let gate = self.gate(&account.id).await;
        let failed = zeroize::Zeroizing::new(failed.access_token.clone());
        let guard = gate.lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut clock = guard;
            let (current, tokens) = snapshot(&store, &account.id)?;
            if tokens.access_token != *failed {
                return Ok(Some((current, tokens)));
            }
            if !is_linked(&current) {
                return Ok(None);
            }
            // 401 재읽기는 요청마다 한 번이며 만료 전 재읽기 제한을 적용하지 않는다.
            let reloaded = reload(&store, &observability, current, "upstream_401", &mut clock)?;
            let (updated, tokens) = snapshot(&store, &reloaded.id)?;
            Ok((tokens.access_token != *failed).then_some((updated, tokens)))
        })
        .await
        .map_err(|_| ())?
    }
}

fn is_linked(account: &Account) -> bool {
    matches!(
        account.source,
        Source::Cpa {
            mode: ImportMode::Linked,
            ..
        }
    )
}

// load와 tokens 사이의 다른 writer 갱신은 고정 횟수만 다시 읽는다.
fn snapshot(store: &AccountStore, id: &str) -> Result<(Account, Tokens), ()> {
    for _ in 0..3 {
        let account = store.load(id).map_err(|_| ())?;
        match store.tokens(&account) {
            Ok(tokens) => return Ok((account, tokens)),
            Err(StoreError::GenerationConflict { .. }) => continue,
            Err(_) => return Err(()),
        }
    }
    Err(())
}

fn reload(
    store: &AccountStore,
    obs: &Observability,
    account: Account,
    trigger: &str,
    clock: &mut ReloadClock,
) -> Result<Account, ()> {
    match linked::reload_and_store(store, &account) {
        Ok(updated) => {
            obs.record_reload(
                &account.id,
                trigger,
                "updated",
                updated.token_meta.expires_at,
            );
            Ok(updated)
        }
        Err(error) => {
            let outcome = match error {
                linked::LinkError::Stale | linked::LinkError::Missing => "link_stale",
                linked::LinkError::InvalidData => "link_invalid",
                linked::LinkError::Store(StoreError::GenerationConflict { .. }) => {
                    "generation_conflict"
                }
                _ => "link_error",
            };
            if outcome != "link_stale" || clock.stale_event() {
                obs.record_reload(&account.id, trigger, outcome, account.token_meta.expires_at);
            }
            // reload 실패에도 실패한 원문이나 파일 경로는 로그에 넣지 않는다.
            Ok(account)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reload_and_stale_intervals_have_independent_boundaries() {
        let mut clock = ReloadClock::default();
        let now = Instant::now();
        assert!(ReloadClock::admit(
            &mut clock.before_expiry,
            now,
            Duration::from_secs(60)
        ));
        assert!(!ReloadClock::admit(
            &mut clock.before_expiry,
            now + Duration::from_secs(59),
            Duration::from_secs(60)
        ));
        assert!(ReloadClock::admit(
            &mut clock.before_expiry,
            now + Duration::from_secs(60),
            Duration::from_secs(60)
        ));
        assert!(ReloadClock::admit(
            &mut clock.stale_event,
            now,
            Duration::from_secs(600)
        ));
        assert!(!ReloadClock::admit(
            &mut clock.stale_event,
            now + Duration::from_secs(599),
            Duration::from_secs(600)
        ));
        assert!(ReloadClock::admit(
            &mut clock.stale_event,
            now + Duration::from_secs(600),
            Duration::from_secs(600)
        ));
    }
}
