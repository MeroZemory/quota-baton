//! Memory-only worker contract. Never serialize or Debug-log these messages.
use crate::dashboard_persistence::Geometry;
use qb_tray::{
    dashboard_model as model,
    state::{Command, DetailTicket},
};
use std::sync::Arc;
#[path = "mailbox.rs"]
mod mailbox;
use mailbox::Mailbox;
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

pub const WAKE: u32 = WM_APP + 0x4c1;
pub trait Bridge: Send + Sync {
    /// Only try_send/coalesce into the existing worker. Never block or do I/O.
    fn enqueue(&self, event: Event);
}
pub enum Event {
    Opened {
        port: UiPort,
    },
    Closed {
        owner: u64,
    },
    Command {
        owner: u64,
        command: Command,
    },
    GeometryDirty {
        owner: u64,
        sequence: u64,
        geometry: Geometry,
    },
    ClockNeeded {
        owner: u64,
    },
}
#[derive(Default)]
pub(super) struct Inbox {
    pub snapshot: Option<(Arc<model::DashboardSnapshot>, model::Clock)>,
    pub detail: Option<(DetailTicket, model::SessionDetail)>,
    pub removed: Vec<String>,
    pub geometry: Option<Option<Geometry>>,
    pub theme: Option<bool>,
    pub clock: Option<model::Clock>,
    pub urgency: Option<qb_tray::state::UrgencyUpdate>,
    snapshot_revision: Option<u64>,
}
#[derive(Clone)]
pub struct UiPort {
    shared: Arc<Mailbox<Inbox>>,
    #[cfg(test)]
    memory_only: bool,
}
impl UiPort {
    pub(super) fn new(owner: u64, hwnd: HWND) -> Self {
        Self {
            shared: Arc::new(Mailbox::new(owner, hwnd.0 as isize)),
            #[cfg(test)]
            memory_only: false,
        }
    }
    /// Synthetic actor-test mailbox. The sentinel is never passed to a Win32 API.
    #[cfg(test)]
    pub(crate) fn test_port(owner: u64) -> Self {
        Self {
            shared: Arc::new(Mailbox::new(owner, 1)),
            memory_only: true,
        }
    }
    /// Consume only the pending snapshot, retaining the revision watermark and
    /// all other pending messages so tests can observe successive publications.
    #[cfg(test)]
    pub(crate) fn test_snapshot(&self) -> Option<Arc<model::DashboardSnapshot>> {
        self.shared
            .drain(self.owner(), |inbox| {
                inbox.snapshot.take().map(|(snapshot, _)| snapshot)
            })
            .flatten()
    }
    pub fn owner(&self) -> u64 {
        self.shared.owner()
    }
    #[cfg(test)]
    pub(crate) fn test_urgency(&self) -> Option<qb_tray::state::UrgencyUpdate> {
        self.shared
            .drain(self.owner(), |inbox| inbox.urgency.take())
            .flatten()
    }
    fn update(&self, change: impl FnOnce(&mut Inbox)) -> bool {
        #[cfg(test)]
        if self.memory_only {
            return self.shared.update(change, |_, _| true);
        }
        self.shared.update(change, |hwnd, owner| {
            let Ok(owner) = usize::try_from(owner) else {
                return false;
            };
            // This is only a nonblocking queue post: no callback or payload pointer.
            unsafe {
                PostMessageW(Some(HWND(hwnd as *mut _)), WAKE, WPARAM(owner), LPARAM(0)).is_ok()
            }
        })
    }
    pub fn snapshot(&self, snapshot: Arc<model::DashboardSnapshot>, clock: model::Clock) -> bool {
        self.update(move |i| {
            if i.snapshot_revision.is_none_or(|n| snapshot.revision >= n) {
                i.snapshot_revision = Some(snapshot.revision);
                i.snapshot = Some((snapshot, clock));
            }
        })
    }
    pub fn detail(&self, ticket: DetailTicket, detail: model::SessionDetail) -> bool {
        self.update(move |i| {
            if i.detail
                .as_ref()
                .is_none_or(|(old, _)| ticket.generation >= old.generation)
            {
                i.detail = Some((ticket, detail));
            }
        })
    }
    pub fn removed(&self, id: String) -> bool {
        self.update(move |i| {
            if i.removed.len() < 2000 && !i.removed.contains(&id) {
                i.removed.push(id);
            }
        })
    }
    pub fn geometry(&self, g: Option<Geometry>) -> bool {
        self.update(move |i| i.geometry = Some(g))
    }
    pub fn theme(&self, dark: bool) -> bool {
        self.update(move |i| i.theme = Some(dark))
    }
    pub fn urgency(&self, update: qb_tray::state::UrgencyUpdate) -> bool {
        self.update(move |i| i.urgency = Some(update))
    }
    pub fn clock(&self, clock: model::Clock) -> bool {
        self.update(move |i| i.clock = Some(clock))
    }
    pub(super) fn take(&self) -> Option<Inbox> {
        self.shared
            .drain(self.owner(), |inbox| {
                if inbox.snapshot.is_none()
                    && inbox.detail.is_none()
                    && inbox.removed.is_empty()
                    && inbox.geometry.is_none()
                    && inbox.theme.is_none()
                    && inbox.clock.is_none()
                    && inbox.urgency.is_none()
                {
                    return None;
                }
                let mut result = std::mem::take(inbox);
                inbox.snapshot_revision = result.snapshot_revision.take();
                Some(result)
            })
            .flatten()
    }
    pub(super) fn close(&self) {
        // The same barrier covers alive check, queued post and close. It is
        // released before any provider disconnect, callbacks or DestroyWindow.
        self.shared.close();
    }
}
