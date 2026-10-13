use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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
            budget: self.clone(),
            bytes,
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
