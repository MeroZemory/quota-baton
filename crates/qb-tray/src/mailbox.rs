//! Pure owner-scoped mailbox synchronization; no Windows calls or I/O.
use std::sync::Mutex;

struct Slot<T> {
    hwnd: isize,
    data: T,
}
pub(super) struct Mailbox<T> {
    owner: u64,
    slot: Mutex<Slot<T>>,
}
impl<T: Default> Mailbox<T> {
    pub(super) fn new(owner: u64, hwnd: isize) -> Self {
        Self {
            owner,
            slot: Mutex::new(Slot {
                hwnd,
                data: T::default(),
            }),
        }
    }
    pub(super) fn owner(&self) -> u64 {
        self.owner
    }
    /// Poster MUST only enqueue a message, never SendMessage/callback/I/O.
    /// Posting and alive check share the close barrier, so no post can start
    /// after close returns and the native owner begins DestroyWindow.
    pub(super) fn update(
        &self,
        change: impl FnOnce(&mut T),
        post: impl FnOnce(isize, u64) -> bool,
    ) -> bool {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.hwnd == 0 {
            return false;
        }
        change(&mut slot.data);
        post(slot.hwnd, self.owner)
    }
    /// A delivered wake waits for the sender's tiny memory-only critical section
    /// instead of consuming the sole wake on try_lock contention.
    pub(super) fn drain<R>(&self, owner: u64, take: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.hwnd == 0 || owner != self.owner {
            return None;
        }
        Some(take(&mut slot.data))
    }
    pub(super) fn close(&self) {
        let discarded = {
            let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
            slot.hwnd = 0;
            std::mem::take(&mut slot.data)
        };
        // Payload destructors and all host callbacks run outside the barrier.
        drop(discarded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };

    #[test]
    fn sole_wake_drains_after_sender_releases_its_post_lock() {
        let mailbox = Arc::new(Mailbox::<Option<u32>>::new(7, 42));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (m, e, r) = (mailbox.clone(), entered.clone(), release.clone());
        let sender = std::thread::spawn(move || {
            m.update(
                |v| *v = Some(19),
                |hwnd, owner| {
                    assert_eq!((hwnd, owner), (42, 7));
                    e.wait();
                    r.wait();
                    true
                },
            )
        });
        entered.wait();
        assert!(mailbox.slot.try_lock().is_err());
        let (attempted, attempts) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let m = mailbox.clone();
        let receiver = std::thread::spawn(move || {
            attempted.send(()).unwrap();
            done.send(m.drain(7, Option::take)).unwrap();
        });
        attempts.recv().unwrap();
        assert!(completed.try_recv().is_err());
        release.wait();
        assert!(sender.join().unwrap());
        assert_eq!(completed.recv().unwrap(), Some(Some(19)));
        receiver.join().unwrap();
        assert_eq!(mailbox.drain(7, Option::take), Some(None));
    }

    #[test]
    fn close_waits_for_post_and_reused_hwnd_rejects_old_owner_wake() {
        let mailbox = Arc::new(Mailbox::<Option<u32>>::new(7, 42));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let destroyed = Arc::new(AtomicBool::new(false));
        let posts = Arc::new(AtomicUsize::new(0));
        let (m, e, r, d, p) = (
            mailbox.clone(),
            entered.clone(),
            release.clone(),
            destroyed.clone(),
            posts.clone(),
        );
        let sender = std::thread::spawn(move || {
            m.update(
                |v| *v = Some(1),
                |_, _| {
                    p.fetch_add(1, Ordering::SeqCst);
                    e.wait();
                    r.wait();
                    assert!(!d.load(Ordering::SeqCst));
                    true
                },
            )
        });
        entered.wait();
        let (attempted, attempts) = mpsc::channel();
        let (m, d) = (mailbox.clone(), destroyed.clone());
        let closer = std::thread::spawn(move || {
            attempted.send(()).unwrap();
            m.close();
            d.store(true, Ordering::SeqCst);
        });
        attempts.recv().unwrap();
        assert!(!destroyed.load(Ordering::SeqCst));
        release.wait();
        assert!(sender.join().unwrap());
        closer.join().unwrap();
        assert!(!mailbox.update(
            |_| panic!("closed payload"),
            |_, _| panic!("post after destroy")
        ));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        let reused = Mailbox::<Option<u32>>::new(8, 42);
        reused.update(|v| *v = Some(2), |_, _| true);
        assert_eq!(reused.drain(7, Option::take), None);
        assert_eq!(reused.drain(8, Option::take), Some(Some(2)));
    }

    #[test]
    fn failed_post_keeps_payload_for_explicit_retry_or_timer_and_close_discards_it() {
        let mailbox = Mailbox::<Option<u32>>::new(7, 42);
        assert!(!mailbox.update(|v| *v = Some(19), |_, _| false));
        assert_eq!(mailbox.drain(7, Option::take), Some(Some(19)));
        mailbox.update(|v| *v = Some(20), |_, _| true);
        mailbox.close();
        assert_eq!(mailbox.drain(7, Option::take), None);
    }
}
