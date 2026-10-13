use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bytes::Bytes;

/// HTTP와 이후 WebSocket/재작성 경로가 함께 사용하는 admission 예산이다.
pub struct InflightBudget {
    limit: usize,
    used: AtomicUsize,
}

impl InflightBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .ok()?;
        Some(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

pub struct Reservation {
    budget: Arc<InflightBudget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Bytes 복제는 같은 raw 저장소를 공유한다. commit 또는 취소가 예산을 반환한다.
pub struct RetainedBody {
    bytes: Bytes,
    _reservation: Reservation,
}

impl RetainedBody {
    pub fn admit(bytes: &Bytes, cap: usize, budget: &Arc<InflightBudget>) -> Option<Self> {
        if bytes.len() > cap {
            return None;
        }
        Some(Self {
            bytes: bytes.clone(),
            _reservation: budget.try_reserve(bytes.len())?,
        })
    }

    pub fn bytes(&self) -> Bytes {
        self.bytes.clone()
    }
}

pub fn mib_bytes(mib: u64) -> usize {
    usize::try_from(mib.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_cap_budget_and_release() {
        let budget = InflightBudget::new(5);
        let bytes = Bytes::from_static(b"raw");
        assert!(RetainedBody::admit(&bytes, 2, &budget).is_none());
        let retained = RetainedBody::admit(&bytes, 3, &budget).unwrap();
        assert_eq!(retained.bytes().as_ptr(), bytes.as_ptr());
        assert_eq!(budget.used(), 3);
        assert!(RetainedBody::admit(&bytes, 3, &budget).is_none());
        drop(retained);
        assert_eq!(budget.used(), 0);
        assert!(budget.try_reserve(usize::MAX).is_none());
        assert!(budget.try_reserve(5).is_some());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn concurrent_admission_never_exceeds_limit() {
        let budget = InflightBudget::new(8);
        let held = budget.try_reserve(7).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let budget = budget.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let reservation = budget.try_reserve(2);
                        assert!(reservation.is_none());
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        drop(held);
        assert_eq!(budget.used(), 0);
    }
}
