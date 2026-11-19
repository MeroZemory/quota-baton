//! Request-local rejected gates, carried to the relay without affecting selection.
use std::{cell::RefCell, collections::BTreeMap};

pub(crate) type Rejections = BTreeMap<String, &'static str>;

thread_local! {
    static ACTIVE: RefCell<Option<Rejections>> = const { RefCell::new(None) };
}

pub(crate) fn capture<T>(run: impl FnOnce() -> T) -> (T, Rejections) {
    struct Scope(Option<Rejections>);
    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _scope = Scope(ACTIVE.with(|slot| slot.replace(Some(Rejections::new()))));
    let result = run();
    let rejections = ACTIVE.with(|slot| slot.take()).unwrap_or_default();
    (result, rejections)
}

pub(crate) fn record(account: &str, gate: &'static str) {
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut()
            && (active.len() < crate::catalog::MAX_ACCOUNTS || active.contains_key(account))
        {
            active.entry(account.to_owned()).or_insert(gate);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r32a_selection_evidence_stays_with_the_request_that_observed_it() {
        record("outside", "ignored");
        let (result, outer) = capture(|| {
            record("cx-a", "catalog_entitlement");
            let ((), inner) = capture(|| record("cx-b", "socket_capacity"));
            assert_eq!(inner, BTreeMap::from([("cx-b".into(), "socket_capacity")]));
            record("cx-a", "later_gate");
            17
        });
        assert_eq!(result, 17);
        assert_eq!(
            outer,
            BTreeMap::from([("cx-a".into(), "catalog_entitlement")])
        );
        assert!(capture(|| ()).1.is_empty());
    }
}
