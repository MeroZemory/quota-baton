//! Cached, flat UIA fragment tree for a drawn flyout. Windows-only staging module.
//! See NOTES.md for publication, epoch dispatch, and HWND teardown obligations.
#![cfg(target_os = "windows")]
#![allow(non_snake_case)]

use std::{
    collections::HashSet,
    ffi::c_void,
    marker::PhantomData,
    mem::{ManuallyDrop, offset_of},
    ptr,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicUsize, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::{
            E_INVALIDARG, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER, HWND, LPARAM, LRESULT,
            SysAllocStringLen, WPARAM,
        },
        System::{
            Com::SAFEARRAY,
            Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement},
            Variant::{InitVariantFromDoubleArray, VARIANT, VT_I4},
        },
        UI::{
            Accessibility::*,
            WindowsAndMessaging::{PostMessageW, WM_APP},
        },
    },
    core::{BOOL, BSTR, Error, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, Result},
};

/// Reserve these messages in the integrating window procedure.
pub const WM_UIA_INVOKE: u32 = WM_APP + 0x3b1;
pub const WM_UIA_FOCUS: u32 = WM_APP + 0x3b2;
/// WPARAM is an opaque request token, LPARAM is the publication epoch. Consume
/// with RootProvider::take_value_request on the owning UI thread.
/// At most eight requests are retained; values contain at most 256 Unicode
/// scalar values (512 UTF-16 units). Successful SetValue only admits a message.
pub const WM_UIA_VALUE: u32 = WM_APP + 0x3b3;

const MAX_VALUE_REQUESTS: usize = 8;
const MAX_VALUE_CHARS: usize = 256;
// Never reuse tokens, including across owners/HWNDs with the same epoch.
static NEXT_VALUE_REQUEST: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Row,
    Text,
    Button,
    Checkbox,
    Tab,
    Edit,
    ComboBox,
    Table,
    ColumnHeader,
    DataRow,
    Chart,
    Group,
}

#[derive(Clone, Debug)]
pub struct Element {
    /// Positive, stable, window-scoped identity; never reassign to another entity.
    pub id: i32,
    /// None means the HWND root. Child IDs refer to the same snapshot only.
    pub parent: Option<i32>,
    /// Already-sanitized accessible_name; rows supply their FULL spec §12 name.
    pub name: String,
    pub kind: Kind,
    /// Physical screen [left, top, width, height], NOT DIPs or right/bottom.
    pub bounds: [f64; 4],
    pub enabled: bool,
    pub action: Option<usize>,
    pub focusable: bool,
    pub focused: bool,
    /// ValuePattern is read-only unless kind is Edit. An Edit with None reads
    /// as an empty string; every other kind requires Some to expose the pattern.
    pub value: Option<String>,
    /// SelectionItem state for Tab/Row/DataRow, or Toggle state for Checkbox.
    /// None withholds those patterns; other kinds do not expose SelectionItem.
    pub selected: Option<bool>,
    /// Some exposes ExpandCollapse, including read-only/non-actionable nodes.
    pub expanded: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub automation_id: &'static str,
    pub name: String,
    pub bounds: [f64; 4],
    /// Reading/navigation order. Later entries win overlapping hit tests, so
    /// inline/menu buttons must follow their containing row in this list.
    pub elements: Vec<Element>,
}

/// Validated, immutable geometry, focus and action indexes from one frame.
/// Clones share the snapshot; changed frame contents require preparation again.
#[derive(Clone, Debug)]
pub struct PreparedSnapshot(Arc<Snapshot>);

pub fn prepare_snapshot(snapshot: Snapshot) -> Result<PreparedSnapshot> {
    validate(&snapshot)?;
    Ok(PreparedSnapshot(Arc::new(snapshot)))
}

struct Cache {
    hwnd: isize,
    alive: bool,
    epoch: usize,
    snapshot: Arc<Snapshot>,
    value_requests: Vec<ValueRequest>,
}

struct ValueRequest {
    token: usize,
    id: i32,
    epoch: usize,
    value: String,
}

/// UI-thread owner. COM clients retain only cached state after this is dropped.
/// !Send/!Sync keeps publication/WM_GETOBJECT/teardown on the owning UI thread.
pub struct RootProvider {
    root: Arc<Node>,
    _ui_thread: PhantomData<Rc<()>>,
}

pub fn create_root_provider(hwnd: isize) -> Result<RootProvider> {
    if hwnd == 0 {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    let cache = Arc::new(Mutex::new(Cache {
        hwnd,
        alive: true,
        epoch: 0,
        snapshot: Arc::new(Snapshot {
            automation_id: "quota-baton.flyout",
            name: String::new(),
            bounds: [0.0; 4],
            elements: Vec::new(),
        }),
        value_requests: Vec::new(),
    }));
    Ok(RootProvider {
        root: Node::new(cache, None, None),
        _ui_thread: PhantomData,
    })
}

impl RootProvider {
    /// Atomically publish geometry, names, focus and frozen action indexes from
    /// one completed frame. Epoch must strictly increase and fit positive LPARAM.
    /// Consume an epoch even when republishing identical geometry/focus. The UI
    /// coordinator must not reset/reuse epochs across replacement owners/HWNDs.
    pub fn publish(&self, snapshot: Snapshot, epoch: usize) -> Result<()> {
        self.publish_prepared(&prepare_snapshot(snapshot)?, epoch)
    }

    /// Reuse a validated frame, consuming a fresh epoch on every publication.
    /// Liveness, epoch checks and pending-value invalidation still apply.
    pub fn publish_prepared(&self, snapshot: &PreparedSnapshot, epoch: usize) -> Result<()> {
        let mut cache = self.root.cache.lock().map_err(|_| unavailable())?;
        if !cache.alive {
            return Err(unavailable());
        }
        if epoch <= cache.epoch || epoch > isize::MAX as usize {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        cache.snapshot = Arc::clone(&snapshot.0);
        cache.epoch = epoch;
        cache.value_requests.clear();
        Ok(())
    }

    /// Consume only this owner's admitted WM_UIA_VALUE request, once. Unknown,
    /// duplicate and obsolete messages return None; a deactivated owner errors.
    /// Publication and teardown discard pending strings. No pointer crosses the
    /// message queue. The caller must also check its current frame/owner/epoch,
    /// resolve the element ID to an Edit target, and apply the string on the UI
    /// thread before publishing again. This method performs no UI or I/O work.
    pub fn take_value_request(
        &self,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<Option<(i32, usize, String)>> {
        let mut cache = self.root.cache.lock().map_err(|_| unavailable())?;
        if !cache.alive {
            return Err(unavailable());
        }
        if wparam.0 == 0 || lparam.0 <= 0 || lparam.0 as usize != cache.epoch {
            return Ok(None);
        }
        let Some(index) = cache
            .value_requests
            .iter()
            .position(|request| request.token == wparam.0 && request.epoch == lparam.0 as usize)
        else {
            return Ok(None);
        };
        let request = cache.value_requests.remove(index);
        if !cache
            .snapshot
            .elements
            .iter()
            .any(|e| e.id == request.id && e.kind == Kind::Edit && e.enabled)
        {
            return Ok(None);
        }
        Ok(Some((request.id, request.epoch, request.value)))
    }

    /// Call only for WM_GETOBJECT; None means let the window's default handler
    /// process other object IDs. Compare the LOW signed 32 bits on 64-bit too.
    pub fn handle_wm_getobject(&self, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if lparam.0 as i32 != UiaRootObjectId {
            return None;
        }
        let hwnd = match self.root.read() {
            Ok(cache) => cache.hwnd,
            Err(_) => return Some(LRESULT(0)),
        };
        let simple = self.simple();
        Some(unsafe {
            UiaReturnRawElementProvider(HWND(hwnd as *mut c_void), wparam, lparam, &simple)
        })
    }

    pub fn simple(&self) -> IRawElementProviderSimple {
        // Slot 0's vtable exactly matches IRawElementProviderSimple.
        unsafe { IRawElementProviderSimple::from_raw(into_slot(self.root.clone(), 0)) }
    }

    /// Call BEFORE DestroyWindow/at the beginning of WM_NCDESTROY, and on hide
    /// if the hidden scene should disappear. Idempotent and terminal: recreate
    /// an owner to reopen. No HWND access happens in this method.
    pub fn deactivate(&self) {
        deactivate_cache(&self.root.cache);
    }

    /// UIA bookkeeping at HWND destruction; this calls deactivate() FIRST. Call
    /// while HWND is STILL this window (never after it can have been reused).
    /// The null-provider call releases UIA's WM_GETOBJECT HWND association.
    pub fn disconnect_before_destroy(&self) -> Result<()> {
        disconnect_cache(
            &self.root.cache,
            || unsafe { UiaDisconnectProvider(&self.simple()) },
            |hwnd| unsafe {
                UiaReturnRawElementProvider(HWND(hwnd as *mut c_void), WPARAM(0), LPARAM(0), None);
            },
        )
    }
}

impl Drop for RootProvider {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Owner-side terminal invalidation must wait for current cached queries, not
/// use the COM callback's try_lock/read path. Capture HWND under the SAME guard.
fn deactivate_cache(cache: &Mutex<Cache>) -> isize {
    let mut cache = match cache.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    let hwnd = cache.hwnd;
    cache.alive = false;
    cache.hwnd = 0;
    cache.snapshot = Arc::new(Snapshot {
        automation_id: cache.snapshot.automation_id,
        name: String::new(),
        bounds: [0.0; 4],
        elements: Vec::new(),
    });
    cache.value_requests.clear();
    cache.value_requests.shrink_to_fit();
    hwnd // Guard is dropped before either cleanup callback can run.
}

fn disconnect_cache(
    cache: &Mutex<Cache>,
    disconnect: impl FnOnce() -> Result<()>,
    null_provider_cleanup: impl FnOnce(isize),
) -> Result<()> {
    let hwnd = deactivate_cache(cache);
    let result = disconnect();
    null_provider_cleanup(hwnd); // Required even when disconnect failed.
    result
}

fn validate(snapshot: &Snapshot) -> Result<()> {
    let mut ids = HashSet::new();
    let mut focus_count = 0;
    if !valid_bounds(snapshot.bounds) {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    for e in &snapshot.elements {
        if e.id <= 0
            || !ids.insert(e.id)
            || !valid_bounds(e.bounds)
            || (e.kind == Kind::Text && (e.action.is_some() || e.focusable || e.focused))
            || (e.focused && (!e.enabled || !e.focusable))
        {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        focus_count += usize::from(e.focused);
    }
    if focus_count > 1 {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    let mut parents: std::collections::HashMap<_, _> = snapshot
        .elements
        .iter()
        .map(|e| (e.id, (e.parent, None::<usize>)))
        .collect();
    for e in &snapshot.elements {
        // Resolve each ancestor once. A fixed path avoids allocating a HashSet
        // for every cell in a large chart's hidden accessibility table.
        let mut path = [0; 65];
        let mut len = 0;
        let mut cursor = e.id;
        let mut depth = loop {
            let &(parent, depth) = parents
                .get(&cursor)
                .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
            if let Some(depth) = depth {
                break depth;
            }
            if len == path.len() || path[..len].contains(&cursor) {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            if let Some(parent) = parent {
                path[len] = cursor;
                len += 1;
                cursor = parent;
            } else {
                parents.get_mut(&cursor).unwrap().1 = Some(0);
                break 0;
            }
        };
        for id in path[..len].iter().rev() {
            depth += 1;
            if depth > 64 {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            parents.get_mut(id).unwrap().1 = Some(depth);
        }
    }
    Ok(())
}

fn valid_bounds(b: [f64; 4]) -> bool {
    b.iter().all(|v| v.is_finite())
        && b[2] >= 0.0
        && b[3] >= 0.0
        && (b[0] + b[2]).is_finite()
        && (b[1] + b[3]).is_finite()
}

fn contains(b: [f64; 4], x: f64, y: f64) -> bool {
    x >= b[0] && y >= b[1] && x < b[0] + b[2] && y < b[1] + b[3]
}

fn unavailable() -> Error {
    uia_error(UIA_E_ELEMENTNOTAVAILABLE)
}
fn uia_error(code: u32) -> Error {
    Error::from_hresult(HRESULT(code as i32))
}

fn string_bstr(value: &str) -> Result<BSTR> {
    if value.is_empty() {
        return Ok(BSTR::new());
    }
    let wide: Vec<u16> = value.encode_utf16().collect();
    // Guard the installed SysAllocStringLen slice wrapper's u32 conversion.
    u32::try_from(wide.len()).map_err(|_| Error::from_hresult(E_INVALIDARG))?;
    // BSTR::from(&str) panics on native allocation failure in this version.
    let bstr = unsafe { SysAllocStringLen(Some(&wide)) };
    if bstr.is_empty() {
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }
    Ok(bstr)
}

fn string_variant(value: &str) -> Result<VARIANT> {
    Ok(VARIANT::from(string_bstr(value)?)) // Moves BSTR ownership.
}

#[derive(Clone, Copy, Default)]
struct Patterns {
    invoke: bool,
    selection_item: bool,
    expand_collapse: bool,
    toggle: bool,
    value: bool,
}

impl Patterns {
    fn of(element: Option<&Element>) -> Self {
        element.map_or_else(Self::default, |e| Self {
            invoke: e.kind != Kind::Text && e.action.is_some(),
            selection_item: matches!(e.kind, Kind::Tab | Kind::Row | Kind::DataRow)
                && e.selected.is_some(),
            expand_collapse: e.expanded.is_some(),
            toggle: e.kind == Kind::Checkbox && e.selected.is_some(),
            value: e.kind == Kind::Edit || e.value.is_some(),
        })
    }

    #[allow(non_upper_case_globals)]
    fn slot(self, id: UIA_PATTERN_ID) -> Option<usize> {
        match id {
            UIA_InvokePatternId if self.invoke => Some(3),
            UIA_SelectionItemPatternId if self.selection_item => Some(4),
            UIA_ExpandCollapsePatternId if self.expand_collapse => Some(5),
            UIA_TogglePatternId if self.toggle => Some(6),
            UIA_ValuePatternId if self.value => Some(7),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Ticket {
    id: i32,
    epoch: usize,
    action: Option<usize>,
    // Freeze QueryInterface support for the lifetime of this COM object. Method
    // calls still validate the current owner/epoch and cached capabilities.
    patterns: Patterns,
}

#[derive(Clone, Copy)]
enum SelectionAction {
    Select,
    Add,
    Remove,
}

// Each COM interface is an address of one of these eight vtable-reference slots.
// repr(C) + offset_of (not a guessed layout) recover the Arc payload from `this`.
// The object stores no HWND wrapper, thread-affine UI state, COM host, child list,
// credentials or callbacks. All slots/vtables are immutable. Arc handles lifetime.
#[repr(C)]
struct Node {
    simple: &'static IRawElementProviderSimple_Vtbl,
    fragment: &'static IRawElementProviderFragment_Vtbl,
    fragment_root: &'static IRawElementProviderFragmentRoot_Vtbl,
    invoke: &'static IInvokeProvider_Vtbl,
    selection_item: &'static ISelectionItemProvider_Vtbl,
    expand_collapse: &'static IExpandCollapseProvider_Vtbl,
    toggle: &'static IToggleProvider_Vtbl,
    value: &'static IValueProvider_Vtbl,
    cache: Arc<Mutex<Cache>>,
    /// Child -> canonical root only. Root never retains children: no cycle.
    root: Option<Arc<Node>>,
    ticket: Option<Ticket>,
}

impl Node {
    fn new(cache: Arc<Mutex<Cache>>, root: Option<Arc<Node>>, ticket: Option<Ticket>) -> Arc<Self> {
        Arc::new(Self {
            simple: &SIMPLE,
            fragment: &FRAGMENT,
            fragment_root: &FRAGMENT_ROOT,
            invoke: &INVOKE,
            selection_item: &SELECTION_ITEM,
            expand_collapse: &EXPAND_COLLAPSE,
            toggle: &TOGGLE,
            value: &VALUE,
            cache,
            root,
            ticket,
        })
    }

    /// COM callbacks never wait on the UI thread or a contended mutex.
    fn read(&self) -> Result<MutexGuard<'_, Cache>> {
        let cache = self.cache.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => uia_error(UIA_E_TIMEOUT),
            TryLockError::Poisoned(_) => unavailable(),
        })?;
        if !cache.alive {
            return Err(unavailable());
        }
        if let Some(ticket) = self.ticket
            && (ticket.epoch != cache.epoch
                || !cache.snapshot.elements.iter().any(|e| e.id == ticket.id))
        {
            return Err(unavailable());
        }
        Ok(cache)
    }

    fn root_arc(&self) -> Arc<Node> {
        match &self.root {
            Some(root) => root.clone(),
            None => {
                // `self` is a live Arc payload throughout every COM callback.
                unsafe {
                    Arc::increment_strong_count(self as *const Node);
                    Arc::from_raw(self as *const Node)
                }
            }
        }
    }

    fn child(&self, e: &Element, epoch: usize) -> Arc<Node> {
        Self::new(
            self.cache.clone(),
            Some(self.root_arc()),
            Some(Ticket {
                id: e.id,
                epoch,
                action: e.action,
                patterns: Patterns::of(Some(e)),
            }),
        )
    }

    fn element<'a>(&self, cache: &'a Cache) -> Result<Option<&'a Element>> {
        match self.ticket {
            None => Ok(None),
            Some(t) => cache
                .snapshot
                .elements
                .iter()
                .find(|e| e.id == t.id)
                .map(Some)
                .ok_or_else(unavailable),
        }
    }

    #[allow(non_upper_case_globals)] // Keep the published Windows constant names.
    fn navigate(&self, direction: NavigateDirection) -> Result<Option<Arc<Node>>> {
        let cache = self.read()?;
        let elements = &cache.snapshot.elements;
        if let Some(ticket) = self.ticket {
            let index = elements
                .iter()
                .position(|e| e.id == ticket.id)
                .ok_or_else(unavailable)?;
            let target = match direction {
                NavigateDirection_Parent => match elements[index].parent {
                    Some(id) => elements.iter().find(|e| e.id == id),
                    None => return Ok(Some(self.root_arc())),
                },
                NavigateDirection_NextSibling => elements[index + 1..]
                    .iter()
                    .find(|e| e.parent == elements[index].parent),
                NavigateDirection_PreviousSibling => elements[..index]
                    .iter()
                    .rev()
                    .find(|e| e.parent == elements[index].parent),
                NavigateDirection_FirstChild => {
                    elements.iter().find(|e| e.parent == Some(ticket.id))
                }
                NavigateDirection_LastChild => {
                    elements.iter().rev().find(|e| e.parent == Some(ticket.id))
                }
                _ => return Err(Error::from_hresult(E_INVALIDARG)),
            };
            Ok(target.map(|e| self.child(e, cache.epoch)))
        } else {
            let target = match direction {
                NavigateDirection_FirstChild => elements.iter().find(|e| e.parent.is_none()),
                NavigateDirection_LastChild => elements.iter().rev().find(|e| e.parent.is_none()),
                NavigateDirection_Parent
                | NavigateDirection_NextSibling
                | NavigateDirection_PreviousSibling => None,
                _ => return Err(Error::from_hresult(E_INVALIDARG)),
            };
            Ok(target.map(|e| self.child(e, cache.epoch)))
        }
    }

    fn hit_test(&self, x: f64, y: f64) -> Result<Option<Arc<Node>>> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let cache = self.read()?;
        if !contains(cache.snapshot.bounds, x, y) {
            return Ok(None);
        }
        Ok(Some(
            match cache
                .snapshot
                .elements
                .iter()
                .rev()
                .find(|e| contains(e.bounds, x, y))
            {
                Some(e) => self.child(e, cache.epoch),
                None => self.root_arc(),
            },
        ))
    }

    fn focus(&self) -> Result<Option<Arc<Node>>> {
        let cache = self.read()?;
        Ok(cache
            .snapshot
            .elements
            .iter()
            .find(|e| e.focused)
            .map(|e| self.child(e, cache.epoch)))
    }

    fn host_bridge<T>(&self, lookup: impl FnOnce(isize) -> Result<T>) -> Result<Option<T>> {
        let hwnd = {
            let cache = self.read()?;
            if self.ticket.is_some() {
                return Ok(None);
            }
            cache.hwnd
        };
        // UIA may re-enter the provider during the native bridge. Never keep
        // the cache locked across that call or block UI-thread teardown on it.
        let provider = lookup(hwnd)?;
        let cache = self.read()?;
        if cache.hwnd != hwnd {
            return Err(unavailable());
        }
        // A concurrently deactivated owner fails read() above, so no host
        // provider obtained through an obsolete/reused HWND escapes to UIA.
        Ok(Some(provider))
    }

    fn bounds(&self) -> Result<[f64; 4]> {
        let cache = self.read()?;
        Ok(self
            .element(&cache)?
            .map_or(cache.snapshot.bounds, |e| e.bounds))
    }

    #[allow(non_upper_case_globals)] // Keep the published Windows constant names.
    fn property(&self, property: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let cache = self.read()?;
        let element = self.element(&cache)?;
        let bounds = element.map_or(cache.snapshot.bounds, |e| e.bounds);
        let patterns = Patterns::of(element);
        Ok(match property {
            UIA_NamePropertyId => {
                string_variant(element.map_or(cache.snapshot.name.as_str(), |e| e.name.as_str()))?
            }
            UIA_ControlTypePropertyId => VARIANT::from(match element.map(|e| e.kind) {
                None => UIA_PaneControlTypeId.0,
                Some(Kind::Row) => UIA_ListItemControlTypeId.0,
                Some(Kind::Text) => UIA_TextControlTypeId.0,
                Some(Kind::Button) => UIA_ButtonControlTypeId.0,
                Some(Kind::Checkbox) => UIA_CheckBoxControlTypeId.0,
                Some(Kind::Tab) => UIA_TabItemControlTypeId.0,
                Some(Kind::Edit) => UIA_EditControlTypeId.0,
                Some(Kind::ComboBox) => UIA_ComboBoxControlTypeId.0,
                Some(Kind::Table) => UIA_TableControlTypeId.0,
                Some(Kind::ColumnHeader) => UIA_HeaderItemControlTypeId.0,
                Some(Kind::DataRow) => UIA_DataItemControlTypeId.0,
                Some(Kind::Chart) => UIA_CustomControlTypeId.0,
                Some(Kind::Group) => UIA_GroupControlTypeId.0,
            }),
            UIA_AutomationIdPropertyId => string_variant(
                match element {
                    None => cache.snapshot.automation_id.to_owned(),
                    Some(e) => format!("quota-baton.element.{}", e.id),
                }
                .as_str(),
            )?,
            UIA_ProcessIdPropertyId => VARIANT::from(std::process::id() as i32),
            UIA_FrameworkIdPropertyId => string_variant("Win32")?,
            UIA_LocalizedControlTypePropertyId
                if element.is_some_and(|e| e.kind == Kind::Chart) =>
            {
                string_variant("차트")?
            }
            UIA_ValueValuePropertyId if patterns.value => {
                string_variant(element.and_then(|e| e.value.as_deref()).unwrap_or(""))?
            }
            UIA_ValueIsReadOnlyPropertyId if patterns.value => {
                VARIANT::from(element.is_none_or(|e| e.kind != Kind::Edit))
            }
            UIA_SelectionItemIsSelectedPropertyId if patterns.selection_item => element
                .and_then(|e| e.selected)
                .map(VARIANT::from)
                .unwrap_or_default(),
            UIA_ExpandCollapseExpandCollapseStatePropertyId => element
                .and_then(|e| e.expanded)
                .map(|expanded| {
                    VARIANT::from(if expanded {
                        ExpandCollapseState_Expanded.0
                    } else {
                        ExpandCollapseState_Collapsed.0
                    })
                })
                .unwrap_or_default(),
            UIA_ToggleToggleStatePropertyId if patterns.toggle => {
                VARIANT::from(if element.and_then(|e| e.selected) == Some(true) {
                    ToggleState_On.0
                } else {
                    ToggleState_Off.0
                })
            }
            UIA_IsEnabledPropertyId => VARIANT::from(element.is_none_or(|e| e.enabled)),
            UIA_IsKeyboardFocusablePropertyId => {
                VARIANT::from(element.is_some_and(|e| e.focusable))
            }
            UIA_HasKeyboardFocusPropertyId => VARIANT::from(element.is_some_and(|e| e.focused)),
            UIA_IsControlElementPropertyId | UIA_IsContentElementPropertyId => VARIANT::from(true),
            UIA_IsInvokePatternAvailablePropertyId => VARIANT::from(patterns.invoke),
            UIA_IsSelectionItemPatternAvailablePropertyId => VARIANT::from(patterns.selection_item),
            UIA_IsExpandCollapsePatternAvailablePropertyId => {
                VARIANT::from(patterns.expand_collapse)
            }
            UIA_IsTogglePatternAvailablePropertyId => VARIANT::from(patterns.toggle),
            UIA_IsValuePatternAvailablePropertyId => VARIANT::from(patterns.value),
            UIA_IsOffscreenPropertyId => VARIANT::from(bounds[2] == 0.0 || bounds[3] == 0.0),
            UIA_NativeWindowHandlePropertyId if element.is_none() => {
                VARIANT::from(cache.hwnd as i32)
            }
            UIA_BoundingRectanglePropertyId => unsafe { InitVariantFromDoubleArray(&bounds)? },
            _ => VARIANT::default(), // VT_EMPTY means unsupported property.
        })
    }

    fn action_ticket(&self, cache: &Cache) -> Result<(usize, usize)> {
        let t = self.ticket.ok_or_else(|| uia_error(UIA_E_NOTSUPPORTED))?;
        if !cache.alive || t.epoch != cache.epoch {
            return Err(unavailable());
        }
        let e = self.element(cache)?.ok_or_else(unavailable)?;
        if !e.enabled {
            return Err(uia_error(UIA_E_ELEMENTNOTENABLED));
        }
        if e.kind == Kind::Text || t.action != e.action {
            return Err(uia_error(UIA_E_NOTSUPPORTED));
        }
        Ok((
            t.action.ok_or_else(|| uia_error(UIA_E_NOTSUPPORTED))?,
            t.epoch,
        ))
    }

    fn post_invoke(&self) -> Result<()> {
        let cache = self.read()?;
        self.post_invoke_cached(&cache)
    }

    fn post_invoke_cached(&self, cache: &Cache) -> Result<()> {
        let (index, epoch) = self.action_ticket(cache)?;
        // Keep the short cache guard through PostMessageW: deactivate/publish
        // cannot invalidate HWND/epoch between validation and enqueueing.
        // UI MUST still compare epoch before resolving the frozen target index.
        post_message(
            cache.hwnd,
            WM_UIA_INVOKE,
            WPARAM(index),
            LPARAM(epoch as isize),
        )
    }

    fn pattern_element<'a>(&self, cache: &'a Cache, id: UIA_PATTERN_ID) -> Result<&'a Element> {
        self.element(cache)?
            .filter(|e| Patterns::of(Some(e)).slot(id).is_some())
            .ok_or_else(|| uia_error(UIA_E_NOTSUPPORTED))
    }

    fn enabled_pattern_element<'a>(
        &self,
        cache: &'a Cache,
        id: UIA_PATTERN_ID,
    ) -> Result<&'a Element> {
        let element = self.pattern_element(cache, id)?;
        if !element.enabled {
            return Err(uia_error(UIA_E_ELEMENTNOTENABLED));
        }
        Ok(element)
    }

    fn selected(&self) -> Result<BOOL> {
        let cache = self.read()?;
        Ok(BOOL::from(
            self.pattern_element(&cache, UIA_SelectionItemPatternId)?
                .selected
                == Some(true),
        ))
    }

    fn selection_container(&self) -> Result<Option<Arc<Node>>> {
        let cache = self.read()?;
        let element = self.pattern_element(&cache, UIA_SelectionItemPatternId)?;
        Ok(Some(match element.parent {
            Some(id) => self.child(
                cache
                    .snapshot
                    .elements
                    .iter()
                    .find(|e| e.id == id)
                    .ok_or_else(unavailable)?,
                cache.epoch,
            ),
            None => self.root_arc(),
        }))
    }

    fn change_selection(&self, action: SelectionAction) -> Result<()> {
        let cache = self.read()?;
        let element = self.enabled_pattern_element(&cache, UIA_SelectionItemPatternId)?;
        let selected = element.selected == Some(true);
        let selecting = !matches!(action, SelectionAction::Remove);
        if selected == selecting {
            return Ok(()); // Selecting an open row must not invoke/close it.
        }
        if !selecting && element.kind == Kind::Tab {
            return Err(uia_error(UIA_E_INVALIDOPERATION)); // A tab cannot be deselected alone.
        }
        if matches!(action, SelectionAction::Add)
            && cache.snapshot.elements.iter().any(|other| {
                other.id != element.id
                    // Session selection spans presentation groups; tabs stay separate.
                    && (element.kind != Kind::Tab || other.parent == element.parent)
                    && Patterns::of(Some(other)).selection_item
                    && (other.kind == Kind::Tab) == (element.kind == Kind::Tab)
                    && other.selected == Some(true)
            })
        {
            return Err(uia_error(UIA_E_INVALIDOPERATION)); // No multiple selection.
        }
        self.post_invoke_cached(&cache)
    }

    fn expanded(&self) -> Result<ExpandCollapseState> {
        let cache = self.read()?;
        Ok(
            if self
                .pattern_element(&cache, UIA_ExpandCollapsePatternId)?
                .expanded
                == Some(true)
            {
                ExpandCollapseState_Expanded
            } else {
                ExpandCollapseState_Collapsed
            },
        )
    }

    fn change_expansion(&self, expanded: bool) -> Result<()> {
        let cache = self.read()?;
        let element = self.enabled_pattern_element(&cache, UIA_ExpandCollapsePatternId)?;
        if element.expanded == Some(expanded) {
            return Ok(());
        }
        self.post_invoke_cached(&cache)
    }

    fn toggle_state(&self) -> Result<ToggleState> {
        let cache = self.read()?;
        Ok(
            if self.pattern_element(&cache, UIA_TogglePatternId)?.selected == Some(true) {
                ToggleState_On
            } else {
                ToggleState_Off
            },
        )
    }

    fn post_toggle(&self) -> Result<()> {
        let cache = self.read()?;
        self.enabled_pattern_element(&cache, UIA_TogglePatternId)?;
        self.post_invoke_cached(&cache)
    }

    fn value(&self) -> Result<BSTR> {
        let cache = self.read()?;
        string_bstr(
            self.pattern_element(&cache, UIA_ValuePatternId)?
                .value
                .as_deref()
                .unwrap_or(""),
        )
    }

    fn value_read_only(&self) -> Result<BOOL> {
        let cache = self.read()?;
        Ok(BOOL::from(
            self.pattern_element(&cache, UIA_ValuePatternId)?.kind != Kind::Edit,
        ))
    }

    unsafe fn post_value(&self, value: PCWSTR) -> Result<()> {
        let mut cache = self.read()?;
        let element = self.enabled_pattern_element(&cache, UIA_ValuePatternId)?;
        if element.kind != Kind::Edit {
            return Err(uia_error(UIA_E_INVALIDOPERATION));
        }
        let id = element.id;
        let epoch = cache.epoch;
        if cache.value_requests.len() >= MAX_VALUE_REQUESTS {
            return Err(uia_error(UIA_E_INVALIDOPERATION));
        }
        let value = unsafe { bounded_value(value)? };
        cache
            .value_requests
            .try_reserve(1)
            .map_err(|_| Error::from_hresult(E_OUTOFMEMORY))?;
        let token = NEXT_VALUE_REQUEST
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| uia_error(UIA_E_INVALIDOPERATION))?;
        cache.value_requests.push(ValueRequest {
            token,
            id,
            epoch,
            value,
        });
        if let Err(error) = post_message(
            cache.hwnd,
            WM_UIA_VALUE,
            WPARAM(token),
            LPARAM(epoch as isize),
        ) {
            cache.value_requests.pop(); // Failed admission never strands a string/queue slot.
            return Err(error);
        }
        Ok(())
    }

    fn post_focus(&self) -> Result<()> {
        let cache = self.read()?;
        let e = self
            .element(&cache)?
            .ok_or_else(|| uia_error(UIA_E_NOTSUPPORTED))?;
        if !e.enabled {
            return Err(uia_error(UIA_E_ELEMENTNOTENABLED));
        }
        if !e.focusable {
            return Err(uia_error(UIA_E_NOTSUPPORTED));
        }
        post_message(
            cache.hwnd,
            WM_UIA_FOCUS,
            WPARAM(e.id as usize),
            LPARAM(cache.epoch as isize),
        )
    }
}

/// The COM caller supplies a valid, NUL-terminated PCWSTR. Bound the scan too,
/// allowing surrogate pairs for all 256 characters. Reject invalid UTF-16 and
/// controls rather than silently changing a requested search value.
unsafe fn bounded_value(value: PCWSTR) -> Result<String> {
    if value.is_null() {
        return Err(Error::from_hresult(E_POINTER));
    }
    let mut wide = [0u16; MAX_VALUE_CHARS * 2];
    for length in 0..=wide.len() {
        let unit = unsafe { value.0.add(length).read() };
        if unit == 0 {
            let mut text = String::new();
            text.try_reserve(length * 3)
                .map_err(|_| Error::from_hresult(E_OUTOFMEMORY))?;
            for (index, character) in char::decode_utf16(wide[..length].iter().copied()).enumerate()
            {
                let character = character.map_err(|_| Error::from_hresult(E_INVALIDARG))?;
                if index >= MAX_VALUE_CHARS || character.is_control() {
                    return Err(Error::from_hresult(E_INVALIDARG));
                }
                text.push(character);
            }
            return Ok(text);
        }
        if length == wide.len() {
            break;
        }
        wide[length] = unit;
    }
    Err(Error::from_hresult(E_INVALIDARG))
}

fn post_message(hwnd: isize, message: u32, wparam: WPARAM, lparam: LPARAM) -> Result<()> {
    #[cfg(test)]
    if let Some(result) = tests::capture_post(hwnd, message, wparam, lparam) {
        return result;
    }
    unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), message, wparam, lparam) }
}

// Raw ABI plumbing is necessary for S_OK + explicitly initialized null outputs
// (no sibling/focus/pattern/host). Generated Result<Interface> trait shims cannot
// express this without leaving the output untouched. No fabricated interfaces.

const OFFSETS: [usize; 8] = [
    offset_of!(Node, simple),
    offset_of!(Node, fragment),
    offset_of!(Node, fragment_root),
    offset_of!(Node, invoke),
    offset_of!(Node, selection_item),
    offset_of!(Node, expand_collapse),
    offset_of!(Node, toggle),
    offset_of!(Node, value),
];

unsafe fn node<'a, const SLOT: usize>(this: *mut c_void) -> &'a Node {
    // Only our exact vtables call this, with a live owning COM reference.
    unsafe { &*this.cast::<u8>().sub(OFFSETS[SLOT]).cast::<Node>() }
}

fn into_slot(value: Arc<Node>, slot: usize) -> *mut c_void {
    let raw = Arc::into_raw(value);
    unsafe { raw.cast::<u8>().add(OFFSETS[slot]).cast_mut().cast() }
}

fn status(result: Result<()>) -> HRESULT {
    match result {
        Ok(()) => HRESULT(0),
        Err(e) => e.code(),
    }
}

unsafe fn output<T>(out: *mut T, value: Result<T>) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    match value {
        Ok(value) => {
            unsafe {
                out.write(value);
            }
            HRESULT(0)
        }
        Err(error) => error.code(),
    }
}

unsafe fn interface_output(
    out: *mut *mut c_void,
    value: Result<Option<Arc<Node>>>,
    slot: usize,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    match value {
        Ok(Some(node)) => {
            unsafe {
                out.write(into_slot(node, slot));
            }
            HRESULT(0)
        }
        Ok(None) => HRESULT(0),
        Err(error) => error.code(),
    }
}

unsafe extern "system" fn query<const S: usize>(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    if iid.is_null() {
        return E_POINTER;
    }
    let n = unsafe { node::<S>(this) };
    let iid = unsafe { *iid };
    let patterns = n.ticket.map_or_else(Patterns::default, |t| t.patterns);
    let slot = if iid == IUnknown::IID || iid == IRawElementProviderSimple::IID {
        0
    } else if iid == IRawElementProviderFragment::IID {
        1
    } else if iid == IRawElementProviderFragmentRoot::IID && n.ticket.is_none() {
        2
    } else if iid == IInvokeProvider::IID && patterns.invoke {
        3
    } else if iid == ISelectionItemProvider::IID && patterns.selection_item {
        4
    } else if iid == IExpandCollapseProvider::IID && patterns.expand_collapse {
        5
    } else if iid == IToggleProvider::IID && patterns.toggle {
        6
    } else if iid == IValueProvider::IID && patterns.value {
        7
    } else {
        return E_NOINTERFACE;
    };
    unsafe {
        Arc::increment_strong_count(n as *const Node);
    }
    let owned = unsafe { Arc::from_raw(n as *const Node) };
    unsafe {
        out.write(into_slot(owned, slot));
    }
    HRESULT(0)
}

unsafe extern "system" fn add_ref<const S: usize>(this: *mut c_void) -> u32 {
    let raw = unsafe { node::<S>(this) } as *const Node;
    unsafe {
        Arc::increment_strong_count(raw);
    }
    let owned = ManuallyDrop::new(unsafe { Arc::from_raw(raw) });
    Arc::strong_count(&owned).min(u32::MAX as usize) as u32
}

unsafe extern "system" fn release<const S: usize>(this: *mut c_void) -> u32 {
    let raw = unsafe { node::<S>(this) } as *const Node;
    let owned = unsafe { Arc::from_raw(raw) };
    let remaining = Arc::strong_count(&owned)
        .saturating_sub(1)
        .min(u32::MAX as usize) as u32;
    drop(owned);
    remaining
}

const fn unknown<const S: usize>() -> IUnknown_Vtbl {
    IUnknown_Vtbl {
        QueryInterface: query::<S>,
        AddRef: add_ref::<S>,
        Release: release::<S>,
    }
}

unsafe extern "system" fn options(this: *mut c_void, out: *mut ProviderOptions) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ProviderOptions(0));
    }
    let result = unsafe { node::<0>(this) }.read().map(|_| {
        ProviderOptions(
            ProviderOptions_ServerSideProvider.0 | ProviderOptions_ProviderOwnsSetFocus.0,
        )
    });
    unsafe { output(out, result) }
}

unsafe extern "system" fn pattern(
    this: *mut c_void,
    id: UIA_PATTERN_ID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    let n = unsafe { node::<0>(this) };
    let result: Result<Option<usize>> = (|| {
        let cache = n.read()?;
        Ok(Patterns::of(n.element(&cache)?).slot(id))
    })();
    match result {
        Ok(Some(slot)) => {
            // Every pattern preserves this COM object's canonical IUnknown.
            unsafe {
                Arc::increment_strong_count(n as *const Node);
            }
            let owned = unsafe { Arc::from_raw(n as *const Node) };
            unsafe { interface_output(out, Ok(Some(owned)), slot) }
        }
        Ok(None) => HRESULT(0),
        Err(error) => error.code(),
    }
}

unsafe extern "system" fn property(
    this: *mut c_void,
    id: UIA_PROPERTY_ID,
    out: *mut VARIANT,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(VARIANT::default());
    }
    unsafe { output(out, node::<0>(this).property(id)) }
}

unsafe extern "system" fn host(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    let n = unsafe { node::<0>(this) };
    let result =
        n.host_bridge(|hwnd| unsafe { UiaHostProviderFromHwnd(HWND(hwnd as *mut c_void)) });
    match result {
        Ok(Some(provider)) => {
            unsafe {
                out.write(provider.into_raw());
            }
            HRESULT(0)
        }
        Ok(None) => HRESULT(0),
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn navigate(
    this: *mut c_void,
    direction: NavigateDirection,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe { interface_output(out, node::<1>(this).navigate(direction), 1) }
}

struct Array(*mut SAFEARRAY);
impl Drop for Array {
    fn drop(&mut self) {
        unsafe {
            let _ = SafeArrayDestroy(self.0);
        }
    }
}

fn runtime_array(id: i32) -> Result<*mut SAFEARRAY> {
    let raw = unsafe { SafeArrayCreateVector(VT_I4, 0, 2) };
    if raw.is_null() {
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }
    let array = Array(raw);
    for (index, value) in [(0i32, UiaAppendRuntimeId as i32), (1, id)] {
        unsafe {
            SafeArrayPutElement(array.0, &index, (&value as *const i32).cast())?;
        }
    }
    let raw = array.0;
    std::mem::forget(array); // Success transfers SAFEARRAY ownership to UIA.
    Ok(raw)
}

unsafe extern "system" fn runtime_id(this: *mut c_void, out: *mut *mut SAFEARRAY) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    let n = unsafe { node::<1>(this) };
    let result = n.read().and_then(|_| match n.ticket {
        Some(t) => runtime_array(t.id),
        None => Ok(ptr::null_mut()), // HWND-hosted root uses the host runtime ID.
    });
    unsafe { output(out, result) }
}

unsafe extern "system" fn bounds(this: *mut c_void, out: *mut UiaRect) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(UiaRect {
            left: 0.0,
            top: 0.0,
            width: 0.0,
            height: 0.0,
        });
    }
    let result = unsafe { node::<1>(this) }.bounds().map(|b| UiaRect {
        left: b[0],
        top: b[1],
        width: b[2],
        height: b[3],
    });
    unsafe { output(out, result) }
}

unsafe extern "system" fn embedded(this: *mut c_void, out: *mut *mut SAFEARRAY) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    status(unsafe { node::<1>(this) }.read().map(|_| ()))
}

unsafe extern "system" fn set_focus(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<1>(this) }.post_focus())
}

unsafe extern "system" fn fragment_root(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let n = unsafe { node::<1>(this) };
    let result = n.read().map(|_| Some(n.root_arc()));
    unsafe { interface_output(out, result, 2) }
}

unsafe extern "system" fn from_point(
    this: *mut c_void,
    x: f64,
    y: f64,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe { interface_output(out, node::<2>(this).hit_test(x, y), 1) }
}

unsafe extern "system" fn get_focus(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { interface_output(out, node::<2>(this).focus(), 1) }
}

unsafe extern "system" fn invoke(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<3>(this) }.post_invoke())
}

unsafe extern "system" fn select(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<4>(this) }.change_selection(SelectionAction::Select))
}

unsafe extern "system" fn add_selection(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<4>(this) }.change_selection(SelectionAction::Add))
}

unsafe extern "system" fn remove_selection(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<4>(this) }.change_selection(SelectionAction::Remove))
}

unsafe fn scalar_output<T>(out: *mut T, initial: T, result: Result<T>) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(initial);
        output(out, result)
    }
}

unsafe extern "system" fn selected(this: *mut c_void, out: *mut BOOL) -> HRESULT {
    unsafe { scalar_output(out, BOOL(0), node::<4>(this).selected()) }
}

unsafe extern "system" fn selection_container(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { interface_output(out, node::<4>(this).selection_container(), 0) }
}

unsafe extern "system" fn expand(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<5>(this) }.change_expansion(true))
}

unsafe extern "system" fn collapse(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<5>(this) }.change_expansion(false))
}

unsafe extern "system" fn expanded(this: *mut c_void, out: *mut ExpandCollapseState) -> HRESULT {
    unsafe {
        scalar_output(
            out,
            ExpandCollapseState_Collapsed,
            node::<5>(this).expanded(),
        )
    }
}

unsafe extern "system" fn toggle(this: *mut c_void) -> HRESULT {
    status(unsafe { node::<6>(this) }.post_toggle())
}

unsafe extern "system" fn toggle_state(this: *mut c_void, out: *mut ToggleState) -> HRESULT {
    unsafe { scalar_output(out, ToggleState_Off, node::<6>(this).toggle_state()) }
}

unsafe extern "system" fn set_value(this: *mut c_void, value: PCWSTR) -> HRESULT {
    status(unsafe { node::<7>(this).post_value(value) })
}

unsafe extern "system" fn value(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(ptr::null_mut());
    }
    let result = unsafe { node::<7>(this) }
        .value()
        .map(|text| text.into_raw().cast_mut().cast());
    unsafe { output(out, result) }
}

unsafe extern "system" fn value_read_only(this: *mut c_void, out: *mut BOOL) -> HRESULT {
    unsafe { scalar_output(out, BOOL(0), node::<7>(this).value_read_only()) }
}

static SIMPLE: IRawElementProviderSimple_Vtbl = IRawElementProviderSimple_Vtbl {
    base__: unknown::<0>(),
    ProviderOptions: options,
    GetPatternProvider: pattern,
    GetPropertyValue: property,
    HostRawElementProvider: host,
};
static FRAGMENT: IRawElementProviderFragment_Vtbl = IRawElementProviderFragment_Vtbl {
    base__: unknown::<1>(),
    Navigate: navigate,
    GetRuntimeId: runtime_id,
    BoundingRectangle: bounds,
    GetEmbeddedFragmentRoots: embedded,
    SetFocus: set_focus,
    FragmentRoot: fragment_root,
};
static FRAGMENT_ROOT: IRawElementProviderFragmentRoot_Vtbl = IRawElementProviderFragmentRoot_Vtbl {
    base__: unknown::<2>(),
    ElementProviderFromPoint: from_point,
    GetFocus: get_focus,
};
static INVOKE: IInvokeProvider_Vtbl = IInvokeProvider_Vtbl {
    base__: unknown::<3>(),
    Invoke: invoke,
};
static SELECTION_ITEM: ISelectionItemProvider_Vtbl = ISelectionItemProvider_Vtbl {
    base__: unknown::<4>(),
    Select: select,
    AddToSelection: add_selection,
    RemoveFromSelection: remove_selection,
    IsSelected: selected,
    SelectionContainer: selection_container,
};
static EXPAND_COLLAPSE: IExpandCollapseProvider_Vtbl = IExpandCollapseProvider_Vtbl {
    base__: unknown::<5>(),
    Expand: expand,
    Collapse: collapse,
    ExpandCollapseState: expanded,
};
static TOGGLE: IToggleProvider_Vtbl = IToggleProvider_Vtbl {
    base__: unknown::<6>(),
    Toggle: toggle,
    ToggleState: toggle_state,
};
static VALUE: IValueProvider_Vtbl = IValueProvider_Vtbl {
    base__: unknown::<7>(),
    SetValue: set_value,
    Value: value,
    IsReadOnly: value_read_only,
};

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use windows::Win32::System::Ole::SafeArrayGetElement;
    use windows::core::BSTR;

    #[derive(Default)]
    struct Posts {
        messages: Vec<(isize, u32, usize, isize)>,
        fail_next: Option<HRESULT>,
    }

    thread_local! {
        static POSTS: RefCell<Option<Posts>> = const { RefCell::new(None) };
    }

    // Only explicitly scoped tests intercept the Win32 enqueue boundary. The
    // raw COM methods/vtables, cache checks and request admission remain real.
    struct PostCapture;
    impl PostCapture {
        fn new() -> Self {
            POSTS.with(|posts| assert!(posts.borrow_mut().replace(Posts::default()).is_none()));
            Self
        }

        fn messages(&self) -> Vec<(isize, u32, usize, isize)> {
            POSTS.with(|posts| posts.borrow().as_ref().unwrap().messages.clone())
        }

        fn fail_next(&self, error: HRESULT) {
            POSTS.with(|posts| posts.borrow_mut().as_mut().unwrap().fail_next = Some(error));
        }
    }
    impl Drop for PostCapture {
        fn drop(&mut self) {
            POSTS.with(|posts| *posts.borrow_mut() = None);
        }
    }

    pub(super) fn capture_post(
        hwnd: isize,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<Result<()>> {
        POSTS.with(|posts| {
            let mut posts = posts.borrow_mut();
            let posts = posts.as_mut()?;
            posts.messages.push((hwnd, message, wparam.0, lparam.0));
            Some(match posts.fail_next.take() {
                Some(error) => Err(Error::from_hresult(error)),
                None => Ok(()),
            })
        })
    }

    fn pattern_element(
        id: i32,
        kind: Kind,
        selected: Option<bool>,
        expanded: Option<bool>,
        value: Option<&str>,
        action: Option<usize>,
    ) -> Element {
        Element {
            id,
            parent: None,
            name: format!("element {id}"),
            kind,
            bounds: [110., 210., 40., 20.],
            enabled: true,
            action,
            focusable: action.is_some() || kind == Kind::Edit,
            focused: false,
            value: value.map(str::to_owned),
            selected,
            expanded,
        }
    }

    fn pattern_scene() -> Snapshot {
        let mut snapshot = scene();
        snapshot.elements = vec![
            pattern_element(1, Kind::Text, Some(false), None, None, None),
            pattern_element(2, Kind::Tab, Some(true), None, None, Some(4)),
            pattern_element(3, Kind::Tab, Some(false), None, None, Some(5)),
            pattern_element(4, Kind::Row, Some(true), Some(true), None, Some(6)),
            pattern_element(5, Kind::DataRow, Some(false), Some(false), None, Some(7)),
            pattern_element(6, Kind::Checkbox, Some(true), None, None, Some(8)),
            pattern_element(7, Kind::Edit, None, None, Some("한글 검색"), None),
            pattern_element(8, Kind::Chart, None, None, Some("52 %"), None),
            pattern_element(9, Kind::Text, None, None, Some("cell"), None),
            pattern_element(10, Kind::Group, Some(false), None, None, None),
            pattern_element(11, Kind::DataRow, Some(false), None, Some("row"), None),
            pattern_element(12, Kind::Edit, None, None, None, None),
            pattern_element(13, Kind::Checkbox, Some(false), None, None, Some(9)),
            pattern_element(14, Kind::Group, None, Some(false), None, None),
            pattern_element(15, Kind::DataRow, None, None, None, None),
        ];
        for element in &mut snapshot.elements {
            if matches!(element.id, 4 | 5 | 11) {
                element.parent = Some(10);
            } else if element.id == 9 {
                element.parent = Some(8);
            }
        }
        snapshot
    }

    fn com_navigate(
        node: &IRawElementProviderFragment,
        direction: NavigateDirection,
    ) -> Result<Option<IRawElementProviderFragment>> {
        let mut raw = ptr::dangling_mut::<c_void>();
        unsafe {
            (node.vtable().Navigate)(node.as_raw(), direction, &mut raw).ok()?;
        }
        Ok(if raw.is_null() {
            None
        } else {
            Some(unsafe { IRawElementProviderFragment::from_raw(raw) })
        })
    }

    fn com_element(owner: &RootProvider, id: i32) -> Result<IRawElementProviderSimple> {
        let expected = format!("quota-baton.element.{id}");
        let mut pending = vec![owner.simple().cast::<IRawElementProviderFragment>()?];
        while let Some(fragment) = pending.pop() {
            let simple = fragment.cast::<IRawElementProviderSimple>()?;
            if BSTR::try_from(&unsafe { simple.GetPropertyValue(UIA_AutomationIdPropertyId)? })?
                == expected.as_str()
            {
                return Ok(simple);
            }
            for direction in [NavigateDirection_NextSibling, NavigateDirection_FirstChild] {
                if let Some(next) = com_navigate(&fragment, direction)? {
                    pending.push(next);
                }
            }
        }
        Err(unavailable())
    }

    fn com_pattern<T: Interface>(
        simple: &IRawElementProviderSimple,
        id: UIA_PATTERN_ID,
    ) -> Result<T> {
        let mut raw = ptr::dangling_mut::<c_void>();
        unsafe {
            (simple.vtable().GetPatternProvider)(simple.as_raw(), id, &mut raw).ok()?;
        }
        if raw.is_null() {
            return Err(Error::from_hresult(E_NOINTERFACE));
        }
        unsafe { IUnknown::from_raw(raw) }.cast::<T>()
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn raw_standard_patterns_match_availability_query_interface_and_identity() -> Result<()> {
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 1)?;
        let weak = Arc::downgrade(&owner.root);
        let patterns = [
            (
                UIA_InvokePatternId,
                UIA_IsInvokePatternAvailablePropertyId,
                IInvokeProvider::IID,
            ),
            (
                UIA_SelectionItemPatternId,
                UIA_IsSelectionItemPatternAvailablePropertyId,
                ISelectionItemProvider::IID,
            ),
            (
                UIA_ExpandCollapsePatternId,
                UIA_IsExpandCollapsePatternAvailablePropertyId,
                IExpandCollapseProvider::IID,
            ),
            (
                UIA_TogglePatternId,
                UIA_IsTogglePatternAvailablePropertyId,
                IToggleProvider::IID,
            ),
            (
                UIA_ValuePatternId,
                UIA_IsValuePatternAvailablePropertyId,
                IValueProvider::IID,
            ),
        ];
        for (id, expected) in [
            (0, [false, false, false, false, false]),
            (1, [false, false, false, false, false]),
            (2, [true, true, false, false, false]),
            (4, [true, true, true, false, false]),
            (5, [true, true, true, false, false]),
            (6, [true, false, false, true, false]),
            (7, [false, false, false, false, true]),
            (8, [false, false, false, false, true]),
            (9, [false, false, false, false, true]),
            (10, [false, false, false, false, false]),
            (11, [false, true, false, false, true]),
            (12, [false, false, false, false, true]),
            (14, [false, false, true, false, false]),
            (15, [false, false, false, false, false]),
        ] {
            let simple = if id == 0 {
                owner.simple()
            } else {
                com_element(&owner, id)?
            };
            let identity = simple.cast::<IUnknown>()?;
            for ((pattern_id, property_id, iid), expected) in patterns.into_iter().zip(expected) {
                assert_eq!(
                    bool::try_from(&unsafe { simple.GetPropertyValue(property_id)? })?,
                    expected
                );
                let mut raw = ptr::dangling_mut::<c_void>();
                unsafe {
                    (simple.vtable().GetPatternProvider)(simple.as_raw(), pattern_id, &mut raw)
                        .ok()?;
                }
                assert_eq!(!raw.is_null(), expected);
                if expected {
                    let pattern = unsafe { IUnknown::from_raw(raw) };
                    assert_eq!(pattern.cast::<IUnknown>()?, identity);
                    assert_eq!(pattern.cast::<IRawElementProviderSimple>()?, simple);
                }
                raw = ptr::dangling_mut::<c_void>();
                let hr = unsafe {
                    (simple.vtable().base__.QueryInterface)(simple.as_raw(), &iid, &mut raw)
                };
                if expected {
                    hr.ok()?;
                    let queried = unsafe { IUnknown::from_raw(raw) };
                    assert_eq!(queried.cast::<IUnknown>()?, identity);
                } else {
                    assert_eq!(hr, E_NOINTERFACE);
                    assert!(raw.is_null());
                }
            }
        }
        for (slot, offset) in OFFSETS.into_iter().enumerate() {
            assert_eq!(offset, slot * std::mem::size_of::<usize>());
        }
        drop(owner);
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn raw_selection_and_expansion_only_invoke_when_the_requested_state_differs() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 1)?;
        let tab = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 2)?,
            UIA_SelectionItemPatternId,
        )?;
        let other_tab = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 3)?,
            UIA_SelectionItemPatternId,
        )?;
        let row_simple = com_element(&owner, 4)?;
        let row = com_pattern::<ISelectionItemProvider>(&row_simple, UIA_SelectionItemPatternId)?;
        let row_expansion =
            com_pattern::<IExpandCollapseProvider>(&row_simple, UIA_ExpandCollapsePatternId)?;
        let other_simple = com_element(&owner, 5)?;
        let other =
            com_pattern::<ISelectionItemProvider>(&other_simple, UIA_SelectionItemPatternId)?;
        let other_expansion =
            com_pattern::<IExpandCollapseProvider>(&other_simple, UIA_ExpandCollapsePatternId)?;
        unsafe {
            assert!(tab.IsSelected()?.as_bool());
            tab.Select()?;
            tab.AddToSelection()?;
            assert_eq!(
                tab.RemoveFromSelection().err().map(|e| e.code()),
                Some(uia_error(UIA_E_INVALIDOPERATION).code())
            );
            other_tab.RemoveFromSelection()?;
            assert_eq!(
                other_tab.AddToSelection().err().map(|e| e.code()),
                Some(uia_error(UIA_E_INVALIDOPERATION).code())
            );
            row.Select()?; // Existing selected detail must stay open.
            row.AddToSelection()?;
            row_expansion.Expand()?;
            other.RemoveFromSelection()?;
            other_expansion.Collapse()?;
            assert!(posts.messages().is_empty());
            assert_eq!(
                tab.SelectionContainer()?.cast::<IUnknown>()?,
                owner.simple().cast::<IUnknown>()?
            );
            assert_eq!(
                BSTR::try_from(
                    &row.SelectionContainer()?
                        .GetPropertyValue(UIA_AutomationIdPropertyId)?
                )?
                .to_string(),
                "quota-baton.element.10"
            );
            other_tab.Select()?;
            row_expansion.Collapse()?;
            row.RemoveFromSelection()?;
            other.Select()?;
            other_expansion.Expand()?;
            assert!(!other.IsSelected()?.as_bool()); // No speculative cache mutation.
            assert_eq!(
                other_expansion.ExpandCollapseState()?,
                ExpandCollapseState_Collapsed
            );
            assert_eq!(
                other.AddToSelection().err().map(|e| e.code()),
                Some(uia_error(UIA_E_INVALIDOPERATION).code())
            );
        }
        assert_eq!(
            posts.messages(),
            [
                (1, WM_UIA_INVOKE, 5, 1),
                (1, WM_UIA_INVOKE, 6, 1),
                (1, WM_UIA_INVOKE, 6, 1),
                (1, WM_UIA_INVOKE, 7, 1),
                (1, WM_UIA_INVOKE, 7, 1),
            ]
        );
        let mut next = pattern_scene();
        for element in &mut next.elements {
            element.selected = element.selected.map(|_| false);
        }
        owner.publish(next, 2)?;
        let tab = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 3)?,
            UIA_SelectionItemPatternId,
        )?;
        let row = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 5)?,
            UIA_SelectionItemPatternId,
        )?;
        unsafe {
            tab.AddToSelection()?;
            row.AddToSelection()?;
        }
        assert_eq!(
            &posts.messages()[5..],
            [(1, WM_UIA_INVOKE, 5, 2), (1, WM_UIA_INVOKE, 7, 2)]
        );
        Ok(())
    }

    #[test]
    fn raw_add_to_selection_rejects_selected_sessions_across_groups() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        let mut snapshot = pattern_scene();
        snapshot
            .elements
            .iter_mut()
            .find(|e| e.id == 5)
            .unwrap()
            .parent = Some(14);
        owner.publish(snapshot.clone(), 1)?;
        let row = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 4)?,
            UIA_SelectionItemPatternId,
        )?;
        let other = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 5)?,
            UIA_SelectionItemPatternId,
        )?;
        unsafe {
            assert_eq!(
                (other.vtable().AddToSelection)(other.as_raw()),
                uia_error(UIA_E_INVALIDOPERATION).code()
            );
            assert!(posts.messages().is_empty());
            assert!(row.IsSelected()?.as_bool());
            assert!(!other.IsSelected()?.as_bool());
            // Select may still replace the selected session in another group.
            (other.vtable().Select)(other.as_raw()).ok()?;
        }
        assert_eq!(posts.messages(), [(1, WM_UIA_INVOKE, 7, 1)]);

        snapshot
            .elements
            .iter_mut()
            .find(|e| e.id == 4)
            .unwrap()
            .selected = Some(false);
        owner.publish(snapshot, 2)?;
        let other = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 5)?,
            UIA_SelectionItemPatternId,
        )?;
        // The selected tab is a separate domain and cannot block this addition.
        unsafe { (other.vtable().AddToSelection)(other.as_raw()).ok()? };
        assert_eq!(
            posts.messages(),
            [(1, WM_UIA_INVOKE, 7, 1), (1, WM_UIA_INVOKE, 7, 2)]
        );
        Ok(())
    }

    #[test]
    fn raw_toggle_and_value_patterns_expose_cached_state_and_read_only_semantic_values()
    -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 9)?;
        for (id, expected, action) in [(6, ToggleState_On, 8), (13, ToggleState_Off, 9)] {
            let simple = com_element(&owner, id)?;
            let toggle = com_pattern::<IToggleProvider>(&simple, UIA_TogglePatternId)?;
            unsafe {
                assert_eq!(toggle.ToggleState()?, expected);
                assert_eq!(
                    i32::try_from(&simple.GetPropertyValue(UIA_ToggleToggleStatePropertyId)?)?,
                    expected.0
                );
                toggle.Toggle()?;
            }
            assert_eq!(
                posts.messages().last(),
                Some(&(1, WM_UIA_INVOKE, action, 9))
            );
        }
        for (id, expected, read_only) in [
            (7, "한글 검색", false),
            (8, "52 %", true),
            (9, "cell", true),
            (11, "row", true),
            (12, "", false),
        ] {
            let simple = com_element(&owner, id)?;
            let value = com_pattern::<IValueProvider>(&simple, UIA_ValuePatternId)?;
            unsafe {
                assert_eq!(value.Value()?.to_string(), expected);
                assert_eq!(value.IsReadOnly()?.as_bool(), read_only);
                assert_eq!(
                    BSTR::try_from(&simple.GetPropertyValue(UIA_ValueValuePropertyId)?)?
                        .to_string(),
                    expected
                );
                assert_eq!(
                    bool::try_from(&simple.GetPropertyValue(UIA_ValueIsReadOnlyPropertyId)?)?,
                    read_only
                );
                assert_eq!(
                    (value.vtable().Value)(value.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                assert_eq!(
                    (value.vtable().IsReadOnly)(value.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                if read_only {
                    assert_eq!(
                        value.SetValue(PCWSTR::null()).err().map(|e| e.code()),
                        Some(uia_error(UIA_E_INVALIDOPERATION).code())
                    );
                }
            }
        }
        let row = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 11)?,
            UIA_SelectionItemPatternId,
        )?;
        let expansion = com_pattern::<IExpandCollapseProvider>(
            &com_element(&owner, 14)?,
            UIA_ExpandCollapsePatternId,
        )?;
        unsafe {
            assert_eq!(
                row.Select().err().map(|e| e.code()),
                Some(uia_error(UIA_E_NOTSUPPORTED).code())
            );
            assert_eq!(
                expansion.Expand().err().map(|e| e.code()),
                Some(uia_error(UIA_E_NOTSUPPORTED).code())
            );
            expansion.Collapse()?;
        }
        assert_eq!(posts.messages().len(), 2);
        Ok(())
    }

    #[test]
    fn raw_value_requests_are_bounded_owner_scoped_and_consumed_exactly_once() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        let other_owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 41)?;
        other_owner.publish(pattern_scene(), 41)?; // Same synthetic HWND/epoch.
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        let requested: Vec<_> = (0..MAX_VALUE_REQUESTS)
            .map(|index| format!("검색 🦀 {index}"))
            .collect();
        for text in &requested {
            let text = wide(text);
            unsafe {
                (value.vtable().SetValue)(value.as_raw(), PCWSTR(text.as_ptr())).ok()?;
            }
        }
        let overflow = wide("overflow");
        assert_eq!(
            unsafe { value.SetValue(PCWSTR(overflow.as_ptr())) }
                .err()
                .map(|e| e.code()),
            Some(uia_error(UIA_E_INVALIDOPERATION).code())
        );
        let messages = posts.messages();
        assert_eq!(messages.len(), MAX_VALUE_REQUESTS);
        assert_eq!(
            messages
                .iter()
                .map(|(_, _, token, _)| *token)
                .collect::<HashSet<_>>()
                .len(),
            MAX_VALUE_REQUESTS
        );
        assert_eq!(unsafe { value.Value()? }.to_string(), "한글 검색");
        assert!(owner.take_value_request(WPARAM(0), LPARAM(41))?.is_none());
        assert!(
            owner
                .take_value_request(WPARAM(messages[0].2), LPARAM(40))?
                .is_none()
        );
        assert!(
            owner
                .take_value_request(WPARAM(usize::MAX), LPARAM(41))?
                .is_none()
        );
        for ((hwnd, message, token, epoch), expected) in messages.into_iter().zip(requested) {
            assert_eq!((hwnd, message, epoch), (1, WM_UIA_VALUE, 41));
            assert!(
                other_owner
                    .take_value_request(WPARAM(token), LPARAM(epoch))?
                    .is_none()
            );
            assert_eq!(
                owner.take_value_request(WPARAM(token), LPARAM(epoch))?,
                Some((7, 41, expected))
            );
            assert!(
                owner
                    .take_value_request(WPARAM(token), LPARAM(epoch))?
                    .is_none()
            );
        }
        let empty = [0u16];
        unsafe {
            value.SetValue(PCWSTR(empty.as_ptr()))?;
        }
        let (_, _, token, epoch) = *posts.messages().last().unwrap();
        assert_eq!(
            owner.take_value_request(WPARAM(token), LPARAM(epoch))?,
            Some((7, 41, String::new()))
        );
        assert!(owner.root.read()?.value_requests.is_empty());
        Ok(())
    }

    #[test]
    fn raw_value_input_scan_unicode_limit_and_failed_enqueue_are_bounded() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 1)?;
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        for text in ["가".repeat(MAX_VALUE_CHARS), "🦀".repeat(MAX_VALUE_CHARS)] {
            let input = wide(&text);
            unsafe {
                value.SetValue(PCWSTR(input.as_ptr()))?;
            }
            let (_, _, token, epoch) = *posts.messages().last().unwrap();
            assert_eq!(
                owner.take_value_request(WPARAM(token), LPARAM(epoch))?,
                Some((7, 1, text))
            );
        }
        for input in [
            wide(&"a".repeat(MAX_VALUE_CHARS + 1)),
            wide(&"🦀".repeat(MAX_VALUE_CHARS + 1)),
            wide("line\nbreak"),
            vec![0xd800, 0],                   // Unpaired high surrogate.
            vec![0xdc00, 0],                   // Unpaired low surrogate.
            vec![65; MAX_VALUE_CHARS * 2 + 1], // No NUL within the bounded scan.
        ] {
            assert_eq!(
                unsafe { value.SetValue(PCWSTR(input.as_ptr())) }
                    .err()
                    .map(|e| e.code()),
                Some(E_INVALIDARG)
            );
        }
        assert_eq!(
            unsafe { value.SetValue(PCWSTR::null()) }
                .err()
                .map(|e| e.code()),
            Some(E_POINTER)
        );
        assert_eq!(posts.messages().len(), 2);
        assert!(owner.root.read()?.value_requests.is_empty());
        posts.fail_next(E_OUTOFMEMORY);
        let input = wide("admission failure");
        assert_eq!(
            unsafe { value.SetValue(PCWSTR(input.as_ptr())) }
                .err()
                .map(|e| e.code()),
            Some(E_OUTOFMEMORY)
        );
        assert!(owner.root.read()?.value_requests.is_empty());
        let (_, _, token, epoch) = *posts.messages().last().unwrap();
        assert!(
            owner
                .take_value_request(WPARAM(token), LPARAM(epoch))?
                .is_none()
        );
        unsafe {
            value.SetValue(PCWSTR(input.as_ptr()))?;
        }
        let (_, _, token, epoch) = *posts.messages().last().unwrap();
        assert_eq!(
            owner.take_value_request(WPARAM(token), LPARAM(epoch))?,
            Some((7, 1, "admission failure".into()))
        );
        Ok(())
    }

    #[test]
    fn raw_patterns_keep_timeout_epoch_enabled_and_terminal_lifetime_guards() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 1)?;
        let row_simple = com_element(&owner, 4)?;
        let row = com_pattern::<ISelectionItemProvider>(&row_simple, UIA_SelectionItemPatternId)?;
        let expansion =
            com_pattern::<IExpandCollapseProvider>(&row_simple, UIA_ExpandCollapsePatternId)?;
        let toggle = com_pattern::<IToggleProvider>(&com_element(&owner, 6)?, UIA_TogglePatternId)?;
        let edit_simple = com_element(&owner, 7)?;
        let value = com_pattern::<IValueProvider>(&edit_simple, UIA_ValuePatternId)?;
        for expected in [uia_error(UIA_E_TIMEOUT).code(), unavailable().code()] {
            let guard = if expected == uia_error(UIA_E_TIMEOUT).code() {
                Some(owner.root.cache.lock().map_err(|_| unavailable())?)
            } else {
                owner.publish(pattern_scene(), 2)?;
                None
            };
            let mut selected = BOOL(1);
            let mut expanded = ExpandCollapseState_Expanded;
            let mut toggled = ToggleState_On;
            let mut read_only = BOOL(1);
            let mut raw = ptr::dangling_mut::<c_void>();
            unsafe {
                assert_eq!(
                    (row.vtable().IsSelected)(row.as_raw(), &mut selected),
                    expected
                );
                assert!(!selected.as_bool());
                assert_eq!(
                    (row.vtable().IsSelected)(row.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                assert_eq!(
                    (row.vtable().SelectionContainer)(row.as_raw(), &mut raw),
                    expected
                );
                assert!(raw.is_null());
                assert_eq!(
                    (row.vtable().SelectionContainer)(row.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                assert_eq!(
                    (expansion.vtable().ExpandCollapseState)(expansion.as_raw(), &mut expanded),
                    expected
                );
                assert_eq!(expanded, ExpandCollapseState_Collapsed);
                assert_eq!(
                    (expansion.vtable().ExpandCollapseState)(expansion.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                assert_eq!(
                    (toggle.vtable().ToggleState)(toggle.as_raw(), &mut toggled),
                    expected
                );
                assert_eq!(toggled, ToggleState_Off);
                assert_eq!(
                    (toggle.vtable().ToggleState)(toggle.as_raw(), ptr::null_mut()),
                    E_POINTER
                );
                raw = ptr::dangling_mut::<c_void>();
                assert_eq!((value.vtable().Value)(value.as_raw(), &mut raw), expected);
                assert!(raw.is_null());
                assert_eq!(
                    (value.vtable().IsReadOnly)(value.as_raw(), &mut read_only),
                    expected
                );
                assert!(!read_only.as_bool());
                assert_eq!(row.Select().err().map(|e| e.code()), Some(expected));
                assert_eq!(row.AddToSelection().err().map(|e| e.code()), Some(expected));
                assert_eq!(
                    row.RemoveFromSelection().err().map(|e| e.code()),
                    Some(expected)
                );
                assert_eq!(expansion.Expand().err().map(|e| e.code()), Some(expected));
                assert_eq!(expansion.Collapse().err().map(|e| e.code()), Some(expected));
                assert_eq!(toggle.Toggle().err().map(|e| e.code()), Some(expected));
                assert_eq!(
                    value.SetValue(PCWSTR::null()).err().map(|e| e.code()),
                    Some(expected)
                );
                raw = ptr::dangling_mut::<c_void>();
                assert_eq!(
                    (edit_simple.vtable().GetPatternProvider)(
                        edit_simple.as_raw(),
                        UIA_ValuePatternId,
                        &mut raw
                    ),
                    expected
                );
                assert!(raw.is_null());
            }
            drop(guard);
        }
        // QI's fixed interface set and IUnknown identity survive epoch expiry.
        assert_eq!(value.cast::<IRawElementProviderSimple>()?, edit_simple);
        assert_eq!(row.cast::<IRawElementProviderSimple>()?, row_simple);
        let mut disabled = pattern_scene();
        for element in &mut disabled.elements {
            element.enabled = false;
        }
        owner.publish(disabled, 3)?;
        let row = com_pattern::<ISelectionItemProvider>(
            &com_element(&owner, 4)?,
            UIA_SelectionItemPatternId,
        )?;
        let expansion = com_pattern::<IExpandCollapseProvider>(
            &com_element(&owner, 4)?,
            UIA_ExpandCollapsePatternId,
        )?;
        let toggle = com_pattern::<IToggleProvider>(&com_element(&owner, 6)?, UIA_TogglePatternId)?;
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        let disabled = Some(uia_error(UIA_E_ELEMENTNOTENABLED).code());
        unsafe {
            assert!(row.IsSelected()?.as_bool());
            assert_eq!(
                expansion.ExpandCollapseState()?,
                ExpandCollapseState_Expanded
            );
            assert_eq!(toggle.ToggleState()?, ToggleState_On);
            assert_eq!(value.Value()?.to_string(), "한글 검색");
            assert!(!value.IsReadOnly()?.as_bool());
            assert_eq!(row.Select().err().map(|e| e.code()), disabled);
            assert_eq!(row.AddToSelection().err().map(|e| e.code()), disabled);
            assert_eq!(row.RemoveFromSelection().err().map(|e| e.code()), disabled);
            assert_eq!(expansion.Expand().err().map(|e| e.code()), disabled);
            assert_eq!(expansion.Collapse().err().map(|e| e.code()), disabled);
            assert_eq!(toggle.Toggle().err().map(|e| e.code()), disabled);
            assert_eq!(
                value.SetValue(PCWSTR::null()).err().map(|e| e.code()),
                disabled
            );
        }
        owner.deactivate();
        unsafe {
            assert_eq!(
                row.Select().err().map(|e| e.code()),
                Some(unavailable().code())
            );
            assert_eq!(
                expansion.Collapse().err().map(|e| e.code()),
                Some(unavailable().code())
            );
            assert_eq!(
                toggle.Toggle().err().map(|e| e.code()),
                Some(unavailable().code())
            );
            assert_eq!(
                value.SetValue(PCWSTR::null()).err().map(|e| e.code()),
                Some(unavailable().code())
            );
        }
        assert!(posts.messages().is_empty());
        Ok(())
    }

    #[test]
    fn raw_value_requests_are_discarded_by_publication_deactivation_and_owner_drop() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        owner.publish(pattern_scene(), 1)?;
        let input = wide("pending search");
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        unsafe {
            value.SetValue(PCWSTR(input.as_ptr()))?;
        }
        assert_eq!(owner.root.read()?.value_requests.len(), 1);
        let (_, _, old_token, old_epoch) = posts.messages()[0];
        assert!(owner.publish(pattern_scene(), 1).is_err());
        assert_eq!(owner.root.read()?.value_requests.len(), 1);
        owner.publish(pattern_scene(), 2)?;
        assert!(owner.root.read()?.value_requests.is_empty());
        assert!(
            owner
                .take_value_request(WPARAM(old_token), LPARAM(old_epoch))?
                .is_none()
        );
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        unsafe {
            value.SetValue(PCWSTR(input.as_ptr()))?;
        }
        let (_, _, token, epoch) = posts.messages()[1];
        assert_ne!(token, old_token);
        let cache = owner.root.cache.clone();
        owner.deactivate();
        {
            let cache = cache.lock().map_err(|_| unavailable())?;
            assert!(cache.value_requests.is_empty());
            assert_eq!(cache.value_requests.capacity(), 0);
        }
        assert_eq!(
            owner
                .take_value_request(WPARAM(token), LPARAM(epoch))
                .err()
                .map(|e| e.code()),
            Some(unavailable().code())
        );
        let replacement = create_root_provider(1)?;
        replacement.publish(pattern_scene(), 2)?;
        assert!(
            replacement
                .take_value_request(WPARAM(token), LPARAM(epoch))?
                .is_none()
        );
        let value =
            com_pattern::<IValueProvider>(&com_element(&replacement, 7)?, UIA_ValuePatternId)?;
        unsafe {
            value.SetValue(PCWSTR(input.as_ptr()))?;
        }
        let (_, _, replacement_token, _) = posts.messages()[2];
        assert_ne!(replacement_token, token);
        assert!(
            replacement
                .take_value_request(WPARAM(token), LPARAM(epoch))?
                .is_none()
        );
        let cache = replacement.root.cache.clone();
        drop(replacement);
        assert!(
            cache
                .lock()
                .map_err(|_| unavailable())?
                .value_requests
                .is_empty()
        );
        assert_eq!(
            unsafe { value.Value() }.err().map(|e| e.code()),
            Some(unavailable().code())
        );
        Ok(())
    }

    #[test]
    fn r1210_prepared_snapshot_rejects_invalid_geometry_focus_and_parents() {
        let mut bounds = scene();
        bounds.bounds[2] = -1.;
        let mut duplicate = scene();
        duplicate.elements[2].id = duplicate.elements[1].id;
        let mut focus = scene();
        focus.elements[2].focused = true;
        let mut missing = scene();
        missing.elements[1].parent = Some(999);
        let mut cycle = scene();
        cycle.elements[0].parent = Some(2);
        cycle.elements[1].parent = Some(1);
        for snapshot in [bounds, duplicate, focus, missing, cycle] {
            assert_eq!(
                prepare_snapshot(snapshot).err().map(|e| e.code()),
                Some(E_INVALIDARG)
            );
        }
    }

    #[test]
    fn r1210_prepared_publication_checks_epochs_and_clears_value_requests() -> Result<()> {
        let posts = PostCapture::new();
        let owner = create_root_provider(1)?;
        let prepared = prepare_snapshot(pattern_scene())?;
        owner.publish_prepared(&prepared, 1)?;
        let value = com_pattern::<IValueProvider>(&com_element(&owner, 7)?, UIA_ValuePatternId)?;
        let input = wide("pending prepared search");
        unsafe { value.SetValue(PCWSTR(input.as_ptr()))? };
        let (_, _, token, epoch) = posts.messages()[0];
        for refused in [0, 1, isize::MAX as usize + 1] {
            assert_eq!(
                owner
                    .publish_prepared(&prepared, refused)
                    .err()
                    .map(|e| e.code()),
                Some(E_INVALIDARG)
            );
            let cache = owner.root.read()?;
            assert_eq!(cache.epoch, 1);
            assert_eq!(cache.value_requests.len(), 1);
            assert!(Arc::ptr_eq(&cache.snapshot, &prepared.0));
        }
        owner.publish_prepared(&prepared, 2)?;
        assert!(owner.root.read()?.value_requests.is_empty());
        assert!(
            owner
                .take_value_request(WPARAM(token), LPARAM(epoch))?
                .is_none()
        );
        assert_eq!(
            unsafe { value.Value() }.err().map(|e| e.code()),
            Some(unavailable().code())
        );
        owner.publish_prepared(&prepared, isize::MAX as usize)?;
        owner.deactivate();
        assert_eq!(
            owner
                .publish_prepared(&prepared, isize::MAX as usize)
                .err()
                .map(|e| e.code()),
            Some(unavailable().code())
        );
        Ok(())
    }

    #[test]
    fn r1210_prepared_snapshots_stay_immutable_across_publication_and_teardown() -> Result<()> {
        let owner = create_root_provider(1)?;
        let other = create_root_provider(1)?;
        let prepared = prepare_snapshot(pattern_scene())?;
        let shared = prepared.clone();
        let weak = Arc::downgrade(&prepared.0);
        owner.publish_prepared(&prepared, 1)?;
        other.publish_prepared(&shared, 1)?;
        let retained = Arc::clone(&owner.root.read()?.snapshot);
        assert!(Arc::ptr_eq(&retained, &prepared.0));
        let before = format!("{retained:?}");
        let child = owner.root.navigate(NavigateDirection_FirstChild)?.unwrap();
        owner.deactivate();
        assert!(child.read().is_err());
        assert_eq!(format!("{retained:?}"), before);
        assert!(Arc::ptr_eq(&other.root.read()?.snapshot, &prepared.0));

        let mut changed = retained.as_ref().clone();
        changed.name = "changed prepared frame".into();
        changed.elements[6].value = Some("changed prepared value".into());
        let next = prepare_snapshot(changed)?;
        other.publish_prepared(&next, 2)?;
        assert!(Arc::ptr_eq(&other.root.read()?.snapshot, &next.0));
        assert_eq!(
            BSTR::try_from(&other.root.property(UIA_NamePropertyId)?)?.to_string(),
            "changed prepared frame"
        );
        let value = com_pattern::<IValueProvider>(&com_element(&other, 7)?, UIA_ValuePatternId)?;
        assert_eq!(
            unsafe { value.Value()? }.to_string(),
            "changed prepared value"
        );
        assert_eq!(format!("{retained:?}"), before);
        drop(prepared);
        drop(shared);
        assert!(weak.upgrade().is_some());
        drop(retained);
        assert!(weak.upgrade().is_none());
        other.deactivate();
        assert!(other.root.read().is_err());
        assert_eq!(next.0.name, "changed prepared frame");
        assert_eq!(next.0.elements.len(), 15);
        Ok(())
    }

    fn scene() -> Snapshot {
        Snapshot {
            automation_id: "quota-baton.flyout",
            name: "계정 사용량".into(),
            bounds: [100.0, 200.0, 360.0, 400.0],
            elements: vec![
                Element {
                    parent: None,
                    id: 1,
                    name: "요약 전체 문장".into(),
                    kind: Kind::Text,
                    bounds: [110.0, 210.0, 340.0, 30.0],
                    enabled: true,
                    action: None,
                    focusable: false,
                    focused: false,
                    value: None,
                    selected: None,
                    expanded: None,
                },
                Element {
                    parent: None,
                    id: 2,
                    name: "계정, Pro, 사용 가능, 5시간 38% 남음, 2시간 초기화".into(),
                    kind: Kind::Row,
                    bounds: [104.0, 250.0, 352.0, 70.0],
                    enabled: true,
                    action: Some(4),
                    focusable: true,
                    focused: true,
                    value: None,
                    selected: None,
                    expanded: None,
                },
                Element {
                    parent: None,
                    id: 3,
                    name: "사유 보기".into(),
                    kind: Kind::Button,
                    bounds: [400.0, 260.0, 40.0, 32.0],
                    enabled: true,
                    action: Some(5),
                    focusable: true,
                    focused: false,
                    value: None,
                    selected: None,
                    expanded: None,
                },
            ],
        }
    }

    #[test]
    fn r10_root_and_fragment_process_ids_identify_the_provider_owner() -> Result<()> {
        let owner = create_root_provider(1)?;
        owner.publish(scene(), 1)?;
        let child = owner.root.navigate(NavigateDirection_FirstChild)?.unwrap();
        for node in [owner.root.as_ref(), child.as_ref()] {
            assert_eq!(
                i32::try_from(&node.property(UIA_ProcessIdPropertyId)?)?,
                std::process::id() as i32,
            );
        }
        Ok(())
    }

    // Fake HWND integer is NEVER passed to a Win32
    // window API: tests call only cache operations, raw COM, and Ole allocators.
    #[test]
    fn dashboard_chart_exposes_hidden_table_children_without_flattening_siblings() -> Result<()> {
        let owner = create_root_provider(1)?;
        let mut s = scene();
        s.automation_id = "quota-baton.dashboard";
        let mut chart = s.elements[2].clone();
        chart.id = 10;
        chart.kind = Kind::Chart;
        chart.action = None;
        chart.focusable = false;
        chart.focused = false;
        chart.name = "합성 계정 오늘 남은 양".into();
        let mut table = chart.clone();
        table.id = 11;
        table.parent = Some(10);
        table.kind = Kind::Table;
        table.bounds = [0.; 4];
        let mut first = table.clone();
        first.id = 12;
        first.parent = Some(11);
        first.kind = Kind::DataRow;
        first.name = "13:10 · 5시간 50% 남음".into();
        let mut second = first.clone();
        second.id = 13;
        second.name = "13:20 · 5시간 45% 남음".into();
        s.elements.truncate(2);
        s.elements.extend([chart, table, first, second]);
        owner.publish(s, 1)?;
        let root_text = owner.root.navigate(NavigateDirection_FirstChild)?.unwrap();
        let account = root_text.navigate(NavigateDirection_NextSibling)?.unwrap();
        let chart = account.navigate(NavigateDirection_NextSibling)?.unwrap();
        assert_eq!(
            i32::try_from(&chart.property(UIA_ControlTypePropertyId)?)?,
            UIA_CustomControlTypeId.0
        );
        let table = chart.navigate(NavigateDirection_FirstChild)?.unwrap();
        let first = table.navigate(NavigateDirection_FirstChild)?.unwrap();
        let second = first.navigate(NavigateDirection_NextSibling)?.unwrap();
        assert!(second.navigate(NavigateDirection_NextSibling)?.is_none());
        assert_eq!(
            BSTR::try_from(&second.property(UIA_NamePropertyId)?)?.to_string(),
            "13:20 · 5시간 45% 남음"
        );
        assert!(bool::try_from(
            &second.property(UIA_IsOffscreenPropertyId)?
        )?);
        assert_eq!(
            first
                .navigate(NavigateDirection_Parent)?
                .unwrap()
                .ticket
                .unwrap()
                .id,
            11
        );
        assert_eq!(
            table
                .navigate(NavigateDirection_Parent)?
                .unwrap()
                .ticket
                .unwrap()
                .id,
            10
        );
        assert!(
            chart
                .navigate(NavigateDirection_Parent)?
                .unwrap()
                .ticket
                .is_none()
        );
        Ok(())
    }
    #[test]
    fn dashboard_parent_cycles_and_missing_parents_refuse_publication() -> Result<()> {
        let owner = create_root_provider(1)?;
        let mut missing = scene();
        missing.elements[1].parent = Some(999);
        assert!(owner.publish(missing, 1).is_err());
        let mut cycle = scene();
        cycle.elements[0].parent = Some(2);
        cycle.elements[1].parent = Some(1);
        assert!(owner.publish(cycle, 1).is_err());
        owner.publish(scene(), 1)?;
        Ok(())
    }
    #[test]
    fn r1210_accessibility_parent_depth_limit_is_order_independent() {
        let mut snapshot = scene();
        snapshot.elements = (1..=65)
            .map(|id| {
                let mut element = pattern_element(id, Kind::Group, None, None, None, None);
                element.parent = (id > 1).then_some(id - 1);
                element
            })
            .collect();
        assert!(validate(&snapshot).is_ok()); // Root plus 64 ancestors.
        snapshot.elements.reverse();
        assert!(validate(&snapshot).is_ok());
        let mut too_deep = pattern_element(66, Kind::Text, None, None, None, None);
        too_deep.parent = Some(65);
        snapshot.elements.push(too_deep);
        assert!(validate(&snapshot).is_err());
        snapshot.elements.reverse();
        assert!(validate(&snapshot).is_err());
    }
    #[test]
    fn r1210_accessibility_shared_ancestors_do_not_hide_broken_components() {
        let mut snapshot = scene();
        snapshot.elements = (1..=10_000)
            .map(|id| {
                let mut element = pattern_element(id, Kind::Group, None, None, None, None);
                element.parent = (id > 1).then_some(1);
                element
            })
            .collect();
        assert!(validate(&snapshot).is_ok());
        snapshot.elements[5000].parent = Some(5002);
        snapshot.elements[5001].parent = Some(5001);
        assert!(validate(&snapshot).is_err());
        snapshot.elements[5001].parent = Some(20_000);
        assert!(validate(&snapshot).is_err());
    }
    #[test]
    fn dashboard_selected_tabs_and_search_value_have_cached_semantics() -> Result<()> {
        let owner = create_root_provider(1)?;
        let mut s = scene();
        s.elements[1].kind = Kind::Tab;
        s.elements[1].selected = Some(true);
        s.elements[2].kind = Kind::Edit;
        s.elements[2].value = Some("한글 검색".into());
        owner.publish(s, 1)?;
        let text = owner.root.navigate(NavigateDirection_FirstChild)?.unwrap();
        let tab = text.navigate(NavigateDirection_NextSibling)?.unwrap();
        let edit = tab.navigate(NavigateDirection_NextSibling)?.unwrap();
        assert!(bool::try_from(
            &tab.property(UIA_SelectionItemIsSelectedPropertyId)?
        )?);
        assert_eq!(
            i32::try_from(&edit.property(UIA_ControlTypePropertyId)?)?,
            UIA_EditControlTypeId.0
        );
        assert_eq!(
            BSTR::try_from(&edit.property(UIA_ValueValuePropertyId)?)?.to_string(),
            "한글 검색"
        );
        assert!(!bool::try_from(
            &edit.property(UIA_ValueIsReadOnlyPropertyId)?
        )?);
        Ok(())
    }
    #[test]
    fn native_host_lookup_releases_cache_and_refuses_a_teardown_during_lookup() -> Result<()> {
        let owner = create_root_provider(1)?;
        owner.publish(scene(), 1)?;
        let result = owner.root.host_bridge(|hwnd| {
            assert_eq!(hwnd, 1);
            assert!(
                owner.root.cache.try_lock().is_ok(),
                "native lookup must be outside the cache lock"
            );
            Ok(42)
        })?;
        assert_eq!(result, Some(42));
        let result = owner.root.host_bridge(|_| {
            deactivate_cache(&owner.root.cache);
            Ok(99)
        });
        assert!(
            result.is_err(),
            "discard a result after the owner was deactivated"
        );
        Ok(())
    }
    #[test]
    fn cached_navigation_properties_and_overlap_hit_test() -> Result<()> {
        let owner = create_root_provider(1)?;
        owner.publish(scene(), 1)?;
        let first = owner
            .root
            .navigate(NavigateDirection_FirstChild)?
            .ok_or_else(unavailable)?;
        assert_eq!(first.ticket.map(|t| t.id), Some(1));
        let row = first
            .navigate(NavigateDirection_NextSibling)?
            .ok_or_else(unavailable)?;
        let name = row.property(UIA_NamePropertyId)?;
        assert_eq!(BSTR::try_from(&name)?.to_string(), scene().elements[1].name);
        assert_eq!(
            i32::try_from(&row.property(UIA_ControlTypePropertyId)?)?,
            UIA_ListItemControlTypeId.0
        );
        {
            let cache = row.read()?;
            assert_eq!(row.action_ticket(&cache)?, (4, 1));
        }
        assert!(!bool::try_from(
            &first.property(UIA_IsInvokePatternAvailablePropertyId)?
        )?);
        assert_eq!(
            owner
                .root
                .hit_test(410.0, 270.0)?
                .and_then(|n| n.ticket)
                .map(|t| t.id),
            Some(3)
        );
        assert_eq!(
            owner.root.focus()?.and_then(|n| n.ticket).map(|t| t.id),
            Some(2)
        );
        assert!(owner.root.hit_test(0.0, 0.0)?.is_none());
        assert!(first.navigate(NavigateDirection_PreviousSibling)?.is_none());
        Ok(())
    }

    #[test]
    fn old_provider_cannot_invoke_reused_index_after_epoch_change_or_teardown() -> Result<()> {
        let owner = create_root_provider(1)?;
        owner.publish(scene(), 40)?;
        let old = owner.root.focus()?.ok_or_else(unavailable)?;
        let mut next = scene();
        next.elements[1].id = 20;
        next.elements[1].name = "다른 계정".into();
        owner.publish(next, 41)?;
        // Both exit before PostMessageW, with the stale-provider HRESULT.
        assert_eq!(
            old.post_invoke().err().map(|e| e.code()),
            Some(unavailable().code())
        );
        assert_eq!(
            old.post_focus().err().map(|e| e.code()),
            Some(unavailable().code())
        );
        assert!(owner.publish(scene(), 41).is_err());
        let mut disabled = scene();
        disabled.elements[1].enabled = false;
        disabled.elements[1].focused = false;
        owner.publish(disabled, 42)?;
        let disabled_row = owner
            .root
            .navigate(NavigateDirection_LastChild)?
            .ok_or_else(unavailable)?
            .navigate(NavigateDirection_PreviousSibling)?
            .ok_or_else(unavailable)?;
        assert_eq!(
            disabled_row.post_invoke().err().map(|e| e.code()),
            Some(uia_error(UIA_E_ELEMENTNOTENABLED).code())
        );
        owner.publish(scene(), 43)?;
        let current = owner.root.focus()?.ok_or_else(unavailable)?;
        owner.deactivate();
        assert_eq!(
            current.post_invoke().err().map(|e| e.code()),
            Some(unavailable().code())
        );
        assert!(owner.root.read().is_err());
        Ok(())
    }

    #[test]
    fn owner_teardown_waits_for_cache_then_attempts_both_cleanups_on_error() -> Result<()> {
        use std::{sync::mpsc, thread, time::Duration};

        let owner = create_root_provider(1)?;
        owner.publish(scene(), 1)?;
        let child = owner.root.focus()?.ok_or_else(unavailable)?;
        let cache = owner.root.cache.clone();
        let query_guard = cache.lock().map_err(|_| unavailable())?;
        let worker_cache = cache.clone();
        let retained_root = owner.root.clone();
        let (contended_tx, contended_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        // This worker only exercises the private cached teardown path with
        // mock cleanup functions. No RootProvider moves off its owning thread,
        // and synthetic HWND 1 never reaches any native/window API.
        let worker = thread::spawn(move || {
            assert!(matches!(
                worker_cache.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            assert!(contended_tx.send(()).is_ok());
            let calls = std::cell::RefCell::new(Vec::new());
            let result = disconnect_cache(
                &worker_cache,
                || {
                    // try_lock proves teardown released its guard before APIs.
                    let state = worker_cache.try_lock().map_err(|_| unavailable())?;
                    assert!(!state.alive);
                    assert_eq!(state.hwnd, 0);
                    assert!(state.snapshot.name.is_empty());
                    assert!(state.snapshot.elements.is_empty());
                    assert_eq!(state.snapshot.bounds, [0.0; 4]);
                    drop(state);
                    assert_eq!(
                        retained_root.read().err().map(|e| e.code()),
                        Some(unavailable().code())
                    );
                    assert_eq!(
                        child.read().err().map(|e| e.code()),
                        Some(unavailable().code())
                    );
                    calls.borrow_mut().push("disconnect");
                    Err(Error::from_hresult(E_NOINTERFACE))
                },
                |hwnd| {
                    assert_eq!(hwnd, 1); // Captured before terminal zeroing.
                    assert!(worker_cache.try_lock().is_ok());
                    calls.borrow_mut().push("null-provider");
                },
            );
            assert_eq!(result.err().map(|e| e.code()), Some(E_NOINTERFACE));
            assert_eq!(calls.into_inner(), ["disconnect", "null-provider"]);
            assert!(done_tx.send(()).is_ok());
        });
        let bound = Duration::from_secs(2);
        assert!(contended_rx.recv_timeout(bound).is_ok());
        // Keep the synthetic query guard held until bounded observation proves
        // teardown did not return early on contention. No sleeps/polling loops.
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(query_guard);
        assert!(done_rx.recv_timeout(bound).is_ok());
        assert!(worker.join().is_ok()); // Completion was bounded before joining.
        assert!(owner.root.read().is_err());
        Ok(())
    }

    #[test]
    fn raw_com_null_outputs_identity_and_safearray_ownership() -> Result<()> {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Node>();
        assert_eq!(OFFSETS[0], 0);
        assert_eq!(OFFSETS[3], 3 * std::mem::size_of::<usize>());
        let owner = create_root_provider(1)?;
        owner.publish(scene(), 1)?;
        let simple = owner.simple();
        let fragment = simple.cast::<IRawElementProviderFragment>()?;
        let root = fragment.cast::<IRawElementProviderFragmentRoot>()?;
        assert_eq!(simple.cast::<IUnknown>()?, root.cast::<IUnknown>()?);
        let first = unsafe { fragment.Navigate(NavigateDirection_FirstChild)? };
        let canonical = unsafe { first.FragmentRoot()? };
        assert_eq!(canonical.cast::<IUnknown>()?, simple.cast::<IUnknown>()?);
        drop(canonical);
        let mut absent = ptr::dangling_mut::<c_void>();
        let hr = unsafe {
            (first.vtable().Navigate)(
                first.as_raw(),
                NavigateDirection_PreviousSibling,
                &mut absent,
            )
        };
        hr.ok()?;
        assert!(absent.is_null());
        let first_simple = first.cast::<IRawElementProviderSimple>()?;
        unsafe {
            (first_simple.vtable().GetPatternProvider)(
                first_simple.as_raw(),
                UIA_InvokePatternId,
                &mut absent,
            )
            .ok()?;
        }
        assert!(absent.is_null());
        assert!(first_simple.cast::<IInvokeProvider>().is_err());
        let array = Array(unsafe { first.GetRuntimeId()? });
        let mut id = 0i32;
        unsafe {
            SafeArrayGetElement(array.0, &1, (&mut id as *mut i32).cast())?;
        }
        assert_eq!(id, 1); // Array guard destroys UIA-transferred test ownership.
        assert!(unsafe { fragment.GetRuntimeId()? }.is_null());
        let mut no_focus = scene();
        no_focus.elements[1].focused = false;
        owner.publish(no_focus, 2)?;
        absent = ptr::dangling_mut::<c_void>();
        unsafe {
            (root.vtable().GetFocus)(root.as_raw(), &mut absent).ok()?;
        }
        assert!(absent.is_null());
        let weak = Arc::downgrade(&owner.root);
        drop(owner);
        assert!(unsafe { first.BoundingRectangle() }.is_err());
        drop(first_simple);
        drop(first);
        drop(root);
        drop(fragment);
        drop(simple);
        assert!(weak.upgrade().is_none());
        Ok(())
    }
}
