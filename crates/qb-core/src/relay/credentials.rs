use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::Mutex;

use crate::{
    accounts::{Account, AccountStore, ImportMode, Source, StoreError, Tokens, linked},
    observability::Observability,
};

#[derive(Default)]
pub(super) struct TokenGates(Mutex<BTreeMap<String, Arc<Mutex<()>>>>);

impl TokenGates {
    async fn gate(&self, id: &str) -> Arc<Mutex<()>> {
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
            let _guard = guard;
            let (mut account, _) = snapshot(&store, &id)?;
            if is_linked(&account)
                && account.token_meta.expires_at.is_some_and(|expiry| {
                    expiry <= chrono::Utc::now() + chrono::Duration::minutes(10)
                })
            {
                account = reload(&store, &observability, account, "before_expiry")?;
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
            let _guard = guard;
            let (current, tokens) = snapshot(&store, &account.id)?;
            if tokens.access_token != *failed {
                return Ok(Some((current, tokens)));
            }
            if !is_linked(&current) {
                return Ok(None);
            }
            let reloaded = reload(&store, &observability, current, "upstream_401")?;
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
            obs.record_reload(&account.id, trigger, outcome, account.token_meta.expires_at);
            // reload 실패에도 실패한 원문이나 파일 경로는 로그에 넣지 않는다.
            Ok(account)
        }
    }
}
