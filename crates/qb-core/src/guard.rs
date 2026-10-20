use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use bytes::Bytes;

/// HTTP와 이후 WebSocket/재작성 경로가 함께 사용하는 admission 예산이다.
pub struct InflightBudget {
    limit: usize,
    used: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl InflightBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        })
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    // 메모리에 이미 들어온 메시지는 초과분도 기록하며 쓰기가 끝나면 돌려준다.
    pub fn reserve_message(self: &Arc<Self>, bytes: usize) -> Reservation {
        self.used.fetch_add(bytes, Ordering::AcqRel);
        Reservation {
            _inner: Arc::new(ReservationInner {
                budget: self.clone(),
                bytes,
            }),
        }
    }

    pub async fn wait_available(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.used() < self.limit {
                return;
            }
            notified.await;
        }
    }

    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .ok()?;
        Some(Reservation {
            _inner: Arc::new(ReservationInner {
                budget: self.clone(),
                bytes,
            }),
        })
    }
}

#[derive(Clone)]
pub struct Reservation {
    _inner: Arc<ReservationInner>,
}
struct ReservationInner {
    budget: Arc<InflightBudget>,
    bytes: usize,
}

impl Drop for ReservationInner {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.changed.notify_waiters();
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

pub fn private_bytes() -> u64 {
    use windows::Win32::System::{
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::GetCurrentProcess,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size,
        )
        .is_ok()
        {
            counters.PrivateUsage as u64
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryGuardTransition {
    Unchanged,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryGuardSample {
    pub private_bytes: u64,
    pub guarded: bool,
    pub transition: MemoryGuardTransition,
}

pub struct MemoryGuard {
    hard_limit: u64,
    guarded: std::sync::atomic::AtomicBool,
    last_private: AtomicU64,
    changed: tokio::sync::Notify,
}

impl MemoryGuard {
    pub fn new(hard_limit: u64) -> Self {
        Self {
            hard_limit,
            guarded: std::sync::atomic::AtomicBool::new(false),
            last_private: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }
    pub fn hard_limit(&self) -> u64 {
        self.hard_limit
    }
    pub fn recovery_limit(&self) -> u64 {
        self.hard_limit.saturating_mul(9) / 10
    }
    pub fn is_guarded(&self) -> bool {
        self.guarded.load(Ordering::Acquire)
    }
    pub async fn changed(&self) {
        self.changed.notified().await;
    }
    pub fn last_private_bytes(&self) -> u64 {
        self.last_private.load(Ordering::Acquire)
    }
    pub fn sample(&self) -> MemoryGuardSample {
        self.apply(private_bytes())
    }
    #[cfg(any(test, feature = "testkit"))]
    pub fn force_sample(&self, bytes: u64) -> MemoryGuardSample {
        self.apply(bytes)
    }
    fn apply(&self, bytes: u64) -> MemoryGuardSample {
        self.last_private.store(bytes, Ordering::Release);
        let was = self.guarded.load(Ordering::Acquire);
        let now = if was {
            bytes >= self.recovery_limit()
        } else {
            bytes > self.hard_limit
        };
        self.guarded.store(now, Ordering::Release);
        if was != now {
            self.changed.notify_waiters();
        }
        MemoryGuardSample {
            private_bytes: bytes,
            guarded: now,
            transition: match (was, now) {
                (false, true) => MemoryGuardTransition::On,
                (true, false) => MemoryGuardTransition::Off,
                _ => MemoryGuardTransition::Unchanged,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn shared_allocation_reservation_lasts_until_every_owner_releases() {
        let budget = super::InflightBudget::new(100);
        let write = budget.reserve_message(100);
        let history = write.clone();
        assert_eq!(budget.used(), 100);
        drop(write);
        assert_eq!(budget.used(), 100);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                budget.wait_available()
            )
            .await
            .is_err()
        );
        drop(history);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            budget.wait_available(),
        )
        .await
        .unwrap();
        assert_eq!(budget.used(), 0);
    }

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

    #[test]
    fn memory_guard_uses_hysteresis_and_reports_transitions() {
        let guard = MemoryGuard::new(1_000);
        assert_eq!(
            guard.force_sample(1_001).transition,
            MemoryGuardTransition::On
        );
        assert!(guard.is_guarded());
        assert_eq!(
            guard.force_sample(950).transition,
            MemoryGuardTransition::Unchanged
        );
        assert_eq!(
            guard.force_sample(899).transition,
            MemoryGuardTransition::Off
        );
        assert!(!guard.is_guarded());
    }
}
