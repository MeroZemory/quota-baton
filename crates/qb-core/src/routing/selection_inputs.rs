//! Bounded native policy snapshot, not a privacy-approved archive.
//! Wire persistence requires the caller's typed ID mapper and privacy review.

use super::{
    ChurnLimits, Mode, On5hLimit, Plan, PolicyStore, Settings, State, pool_id, session_id,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
};

/// Raw ID links: alias choice is deferred until pure policy evaluation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyKeyInput {
    pub key: Option<String>,
    pub exact: String,
    pub legacy: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementInput {
    pub from: String,
    pub to: String,
    pub at: DateTime<Utc>,
    pub discretionary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInput {
    pub project: Option<String>,
    pub baseline: Settings,
    pub explicit: Option<Settings>,
    pub anchor: Option<String>,
    pub urgency: Option<On5hLimit>,
    pub movements: Vec<MovementInput>,
}

impl SessionInput {
    fn settings(&self, defaults: &Settings) -> Settings {
        let mut settings = self
            .explicit
            .as_ref()
            .unwrap_or(if self.project.is_some() {
                &self.baseline
            } else {
                defaults
            })
            .clone();
        if let Some(urgency) = self.urgency {
            settings.on_5h_limit = urgency;
        }
        settings
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRowsInput {
    pub sessions: BTreeMap<String, SessionInput>,
    pub legacy_codex_roots: BTreeSet<String>,
}

impl PolicyRowsInput {
    fn capture(state: &State, scope: &PolicyKeyInput) -> Self {
        let ids = std::iter::once(&scope.exact).chain(scope.legacy.iter());
        let mut sessions = BTreeMap::new();
        let mut legacy_codex_roots = BTreeSet::new();
        for id in ids {
            if let Some(row) = state.sessions.get(id) {
                sessions.insert(
                    id.clone(),
                    SessionInput {
                        project: row.project.clone(),
                        baseline: row.baseline.clone(),
                        explicit: row.explicit.clone(),
                        anchor: row.anchor.clone(),
                        urgency: row.urgency,
                        movements: row
                            .movements
                            .iter()
                            .map(|m| MovementInput {
                                from: m.from.clone(),
                                to: m.to.clone(),
                                at: m.at,
                                discretionary: m.discretionary,
                            })
                            .collect(),
                    },
                );
            }
            if state.legacy_codex_roots.contains(id) {
                legacy_codex_roots.insert(id.clone());
            }
        }
        Self {
            sessions,
            legacy_codex_roots,
        }
    }

    fn policy_id<'a>(&self, scope: &'a PolicyKeyInput) -> &'a str {
        if !self.sessions.contains_key(&scope.exact)
            && let Some(legacy) = &scope.legacy
            && self.legacy_codex_roots.contains(legacy)
        {
            return legacy;
        }
        &scope.exact
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionPolicyInput {
    pub scope: PolicyKeyInput,
    pub defaults: Settings,
    pub limits: ChurnLimits,
    pub revision: u64,
    pub state: PolicyRowsInput,
    pub witness: PolicyRowsInput,
    pub frozen: bool,
    pub history_overflow_until: Option<DateTime<Utc>>,
    pub recovery_blocked: bool,
    pub lifecycle_recovery: bool,
    pub pool_moves: BTreeMap<String, Vec<DateTime<Utc>>>,
    /// Account-to-native-pool relation; preserve this link through ID mapping.
    pub target_pools: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyInputRefusal {
    MissingTargetPool,
}

impl PolicyStore {
    pub(crate) fn selection_inputs(
        &self,
        key: Option<&str>,
        accounts: &[String],
    ) -> SelectionPolicyInput {
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let witness = self
            .recovery_intent
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let scope = PolicyKeyInput {
            key: key.map(str::to_owned),
            exact: key.map(session_id).unwrap_or_default(),
            legacy: key
                .filter(|key| key.starts_with("codex:") && key.contains(":agent:"))
                .map(|key| session_id(key.split(":agent:").next().unwrap_or(key))),
        };
        let target_pools: BTreeMap<_, _> = accounts
            .iter()
            .map(|account| (account.clone(), pool_id(account).to_owned()))
            .collect();
        let pool_moves = target_pools
            .values()
            .filter_map(|pool| {
                state
                    .pool_moves
                    .get(pool)
                    .map(|times| (pool.clone(), times.clone()))
            })
            .collect();
        SelectionPolicyInput {
            state: PolicyRowsInput::capture(&state, &scope),
            witness: PolicyRowsInput::capture(&witness, &scope),
            scope,
            defaults: self.defaults.clone(),
            limits: self.limits.clone(),
            revision: state.revision,
            frozen: state.frozen,
            history_overflow_until: state.history_overflow_until,
            recovery_blocked: self.recovery_blocked.load(Ordering::Acquire),
            lifecycle_recovery: self.lifecycle_recovery.load(Ordering::Acquire),
            pool_moves,
            target_pools,
        }
    }
}

impl SelectionPolicyInput {
    fn selected_id(&self) -> &str {
        let current = self.state.policy_id(&self.scope);
        if self.scope.key.is_some() && self.recovery_blocked {
            let guarded = self.witness.policy_id(&self.scope);
            if self.witness.sessions.contains_key(guarded) {
                return guarded;
            }
        }
        current
    }

    pub fn plan(&self) -> Plan {
        let id = self.selected_id();
        let session = if self.recovery_blocked {
            self.witness
                .sessions
                .get(id)
                .or_else(|| self.state.sessions.get(id))
        } else {
            self.state.sessions.get(id)
        };
        Plan {
            settings: session.map_or_else(|| self.defaults.clone(), |s| s.settings(&self.defaults)),
            anchor: session.and_then(|s| s.anchor.clone()),
            project: session.and_then(|s| s.project.clone()),
            source: session.map_or("global", |s| {
                if s.explicit.is_some() || s.urgency.is_some() {
                    "session"
                } else if s.project.is_some() {
                    "project"
                } else {
                    "global"
                }
            }),
            revision: self.revision,
        }
    }

    pub fn placement_plan(&self) -> (Plan, bool) {
        let exists = self.scope.key.is_none()
            || self.frozen
            || self.recovery_blocked
            || self
                .state
                .sessions
                .contains_key(self.state.policy_id(&self.scope))
            || self
                .witness
                .sessions
                .contains_key(self.witness.policy_id(&self.scope));
        (self.plan(), exists)
    }

    pub fn permits_dispatch(&self, account: &str) -> bool {
        if self.frozen {
            return false;
        }
        if self.scope.key.is_none() {
            return self.defaults.mode != Mode::Strict;
        }
        let plan = self.plan();
        plan.settings.mode != Mode::Strict
            || (!self.recovery_blocked
                && !self.lifecycle_recovery
                && plan.anchor.as_deref() == Some(account))
    }

    pub fn discretionary_allowed(
        &self,
        target: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool, PolicyInputRefusal> {
        // Missing raw relations are malformed input even if another guard vetoes.
        let target_pool = target
            .map(|account| {
                self.target_pools
                    .get(account)
                    .ok_or(PolicyInputRefusal::MissingTargetPool)
            })
            .transpose()?;
        if self.history_overflow_until.is_some_and(|until| now < until) {
            return Ok(false);
        }
        let pool_moves = target_pool
            .and_then(|pool| self.pool_moves.get(pool))
            .into_iter()
            .flatten()
            .filter(|at| (now - **at).num_seconds() < self.limits.window_secs as i64)
            .count();
        if pool_moves >= self.limits.max_pool_moves {
            return Ok(false);
        }
        let Some(session) = self.state.sessions.get(self.state.policy_id(&self.scope)) else {
            return Ok(self.defaults.mode == Mode::Rebalance);
        };
        Ok(session.settings(&self.defaults).mode == Mode::Rebalance
            && session
                .movements
                .iter()
                .filter(|m| {
                    m.discretionary && (now - m.at).num_seconds() < self.limits.window_secs as i64
                })
                .count()
                < self.limits.max_group_moves
            && !session.movements.iter().any(|m| {
                (m.discretionary && (now - m.at).num_seconds() < self.limits.dwell_secs as i64)
                    || target.is_some_and(|target| {
                        target == m.from
                            && (now - m.at).num_seconds() < self.limits.revisit_secs as i64
                    })
            }))
    }
}

#[cfg(test)]
mod tests;
