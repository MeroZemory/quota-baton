// Pump-local witness only; no poll, owner, publication or transport authority.
// Deliberately private child module. Wiring and runtime acceptance are separate.
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    account: String,
    closed: bool,
    tainted: bool,
    current: Option<Attempt>,
}
struct Attempt {
    key: Option<String>,
    model: Option<String>,
    sent: Sent,
    flushed: bool,
    claimed: bool,
}
#[derive(Clone)]
pub(super) struct Leg(Arc<Mutex<State>>);
#[derive(Clone)]
pub(super) struct Sent(Arc<()>);
pub(super) struct WriteTicket {
    leg: Leg,
    sent: Sent,
    flushed: bool,
}
pub(crate) struct Candidate {
    leg: Leg,
    sent: Sent,
}
impl Sent {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Leg {
    pub(super) fn new(account: &str) -> Self {
        Self(Arc::new(Mutex::new(State {
            account: account.to_owned(),
            ..State::default()
        })))
    }
    // Every initial/resend create uses this SAME boundary: first start_send
    // after poll_ready, never write-future allocation. None denies witness only,
    // not ordinary forwarding. Opaque possible-create data calls ambiguous().
    pub(super) fn start(
        &self,
        key: Option<&str>,
        model: Option<&str>,
    ) -> Option<(Sent, WriteTicket)> {
        let mut state = self.0.lock().ok()?;
        if state.closed || state.tainted {
            return None;
        }
        if state.current.is_some() {
            state.tainted = true;
            return None;
        }
        let sent = Sent(Arc::new(()));
        state.current = Some(Attempt {
            key: key.map(str::to_owned),
            model: model.map(str::to_owned),
            sent: sent.clone(),
            flushed: false,
            claimed: false,
        });
        Some((
            sent.clone(),
            WriteTicket {
                leg: self.clone(),
                sent,
                flushed: false,
            },
        ))
    }
    pub(super) fn ambiguous(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.tainted = true;
        }
    }
    // Close before physical replacement, cancellation or shutdown. A fresh
    // Leg is required per physical upgrade; Clone still denotes the SAME leg.
    pub(super) fn close(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.closed = true;
        }
    }
    // Caller MUST establish exact terminal attribution. Never use created,
    // first-sent fallback or adjacency. Resolve quota candidate before retire.
    pub(super) fn retire(&self, sent: &Sent) {
        if let Ok(mut state) = self.0.lock() {
            if state.current.as_ref().is_some_and(|a| a.sent.matches(sent)) {
                state.current = None;
            }
        }
    }
    pub(super) fn quota_candidate(&self) -> Option<Candidate> {
        let mut state = self.0.lock().ok()?;
        if state.closed || state.tainted {
            return None;
        }
        let attempt = state.current.as_mut()?;
        if !attempt.flushed || attempt.claimed {
            return None;
        }
        attempt.claimed = true;
        Some(Candidate {
            leg: self.clone(),
            sent: attempt.sent.clone(),
        })
    }
}
impl WriteTicket {
    // Call only after successful LAST-fragment flush. Failed start_send,
    // partial/failed flush, cancellation and abort drop the unfinished ticket.
    pub(super) fn flush(mut self) {
        if let Ok(mut state) = self.leg.0.lock() {
            if let Some(attempt) = state.current.as_mut() {
                if attempt.sent.matches(&self.sent) {
                    attempt.flushed = true;
                    self.flushed = true;
                }
            }
        }
    }
}
impl Drop for WriteTicket {
    fn drop(&mut self) {
        if !self.flushed {
            self.leg.ambiguous();
        }
    }
}
impl Candidate {
    // Final synchronous registration boundary, NOT a snapshot or proof mint.
    // Candidate is consumed even on decline. The witness lock stays held until
    // register RETURNS: overlap/close/retire cannot interleave with registration.
    // Acquire Store/runtime/binding/policy gates BEFORE this boundary. Inside,
    // only the existing bounded proof/deferred leaf locks may be acquired.
    // No helper reentry, outer-gate acquisition, I/O, await or deferred future.
    // Closure must recheck original poll receipt/owner/policy/cancellation under
    // their own gates and return the existing RAII registration result directly.
    // This serializes only helper transitions, not external cancellation gates
    // or later physical delivery. Caller still owns final handoff and rollback.
    pub(crate) fn with_current<R>(
        self,
        expected_account: &str,
        expected_key: Option<&str>,
        expected_model: Option<&str>,
        register: impl FnOnce() -> R,
    ) -> Option<R> {
        let state = self.leg.0.lock().ok()?;
        if state.closed || state.tainted || state.account != expected_account {
            return None;
        }
        let attempt = state.current.as_ref()?;
        if !attempt.sent.matches(&self.sent)
            || !attempt.flushed
            || !attempt.claimed
            || attempt.key.as_deref() != expected_key
            || attempt.model.as_deref() != expected_model
        {
            return None;
        }
        let result = register();
        drop(state);
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flushed(leg: &Leg) -> Sent {
        let (sent, write) = leg
            .start(Some("codex:sample-root"), Some("sample-model"))
            .unwrap();
        write.flush();
        sent
    }

    #[test]
    fn receipt_from_another_account_cannot_register_on_this_leg() {
        let leg = Leg::new("sample-source");
        flushed(&leg);
        assert!(
            leg.quota_candidate()
                .unwrap()
                .with_current(
                    "sample-other",
                    Some("codex:sample-root"),
                    Some("sample-model"),
                    || panic!("wrong account")
                )
                .is_none()
        );
        assert!(
            leg.quota_candidate().is_none(),
            "decline still spends this claim"
        );
    }

    #[test]
    fn request_identity_is_captured_at_physical_send_not_poll_completion() {
        for (key, model) in [
            (Some("codex:other-root"), Some("sample-model")),
            (Some("codex:sample-root"), Some("other-model")),
            (None, None),
        ] {
            let leg = Leg::new("sample-acct");
            flushed(&leg);
            assert!(
                leg.quota_candidate()
                    .unwrap()
                    .with_current("sample-acct", key, model, || panic!("wrong request"))
                    .is_none()
            );
        }
    }

    #[test]
    fn final_registration_holds_lock_and_duplicate_claims_stay_spent() {
        let leg = Leg::new("sample-acct");
        flushed(&leg);
        let candidate = leg.quota_candidate().unwrap();
        assert!(leg.quota_candidate().is_none());
        assert_eq!(
            candidate.with_current(
                "sample-acct",
                Some("codex:sample-root"),
                Some("sample-model"),
                || {
                    assert!(matches!(
                        leg.0.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ));
                    7
                }
            ),
            Some(7)
        );
        assert!(leg.0.try_lock().is_ok());
        assert!(leg.quota_candidate().is_none());
    }

    #[test]
    fn initial_or_resend_overlap_burns_before_or_after_flush() {
        for finish_first in [false, true] {
            let leg = Leg::new("sample-acct");
            let (sent, write) = leg
                .start(Some("codex:sample-root"), Some("sample-model"))
                .unwrap();
            let candidate = if finish_first {
                write.flush();
                Some(leg.quota_candidate().unwrap())
            } else {
                assert!(leg.quota_candidate().is_none());
                assert!(
                    leg.start(Some("codex:sample-root"), Some("sample-model"))
                        .is_none()
                );
                write.flush();
                None
            };
            assert!(
                leg.start(Some("codex:sample-root"), Some("sample-model"))
                    .is_none()
            );
            if let Some(candidate) = candidate {
                assert!(
                    candidate
                        .with_current(
                            "sample-acct",
                            Some("codex:sample-root"),
                            Some("sample-model"),
                            || panic!("stale registration")
                        )
                        .is_none()
                );
            }
            leg.retire(&sent);
            assert!(
                leg.start(Some("codex:sample-root"), Some("sample-model"))
                    .is_none()
            );
            assert!(leg.quota_candidate().is_none());
        }
    }

    #[test]
    fn failed_start_partial_or_failed_flush_and_abort_burn() {
        // All unfinished-write failure routes must drop this ticket in the pump.
        // This tests that common transition, not the separate transport routes.
        let leg = Leg::new("sample-acct");
        let (sent, write) = leg
            .start(Some("codex:sample-root"), Some("sample-model"))
            .unwrap();
        drop(write);
        leg.retire(&sent);
        assert!(
            leg.start(Some("codex:sample-root"), Some("sample-model"))
                .is_none()
        );
        assert!(leg.quota_candidate().is_none());
    }

    #[test]
    fn cancellation_or_leg_change_revokes_pending_candidate() {
        let old = Leg::new("sample-acct");
        flushed(&old);
        let candidate = old.quota_candidate().unwrap();
        old.close();
        let next = Leg::new("sample-acct");
        flushed(&next);
        assert!(
            candidate
                .with_current(
                    "sample-acct",
                    Some("codex:sample-root"),
                    Some("sample-model"),
                    || panic!("old leg")
                )
                .is_none()
        );
        assert!(
            old.start(Some("codex:sample-root"), Some("sample-model"))
                .is_none()
        );
        assert_eq!(
            next.quota_candidate().unwrap().with_current(
                "sample-acct",
                Some("codex:sample-root"),
                Some("sample-model"),
                || true
            ),
            Some(true)
        );
    }

    #[test]
    fn only_exact_terminal_retires_and_stale_terminal_cannot_clear_resend() {
        let leg = Leg::new("sample-acct");
        let first = flushed(&leg);
        let candidate = leg.quota_candidate().unwrap();
        let other = Leg::new("sample-acct");
        let unrelated = flushed(&other);
        leg.retire(&unrelated);
        assert_eq!(
            candidate.with_current(
                "sample-acct",
                Some("codex:sample-root"),
                Some("sample-model"),
                || true
            ),
            Some(true)
        );
        leg.retire(&first);
        let second = flushed(&leg);
        assert!(!first.matches(&second));
        leg.retire(&first);
        assert!(
            leg.quota_candidate()
                .unwrap()
                .with_current(
                    "sample-acct",
                    Some("codex:sample-root"),
                    Some("sample-model"),
                    || true
                )
                .is_some()
        );
    }

    #[test]
    fn retirement_invalidates_old_candidate_and_abandonment_does_not_refund() {
        let leg = Leg::new("sample-acct");
        let first = flushed(&leg);
        let old = leg.quota_candidate().unwrap();
        leg.retire(&first);
        flushed(&leg);
        assert!(
            old.with_current(
                "sample-acct",
                Some("codex:sample-root"),
                Some("sample-model"),
                || panic!("retired attempt")
            )
            .is_none()
        );
        drop(leg.quota_candidate().unwrap());
        assert!(leg.quota_candidate().is_none());
    }

    #[test]
    fn ambiguous_data_and_cancelled_write_revoke_without_revival() {
        let leg = Leg::new("sample-acct");
        flushed(&leg);
        let candidate = leg.quota_candidate().unwrap();
        leg.ambiguous();
        assert!(
            candidate
                .with_current(
                    "sample-acct",
                    Some("codex:sample-root"),
                    Some("sample-model"),
                    || panic!("ambiguous")
                )
                .is_none()
        );
        let next = Leg::new("sample-acct");
        let (_, write) = next
            .start(Some("codex:sample-root"), Some("sample-model"))
            .unwrap();
        next.close();
        write.flush();
        assert!(next.quota_candidate().is_none());
        assert!(
            next.start(Some("codex:sample-root"), Some("sample-model"))
                .is_none()
        );
    }
}
