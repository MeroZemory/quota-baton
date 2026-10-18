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
    sync::{Arc, Mutex, MutexGuard, TryLockError},
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
    core::{BSTR, Error, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Result},
};

/// Reserve these two messages in the integrating window procedure.
pub const WM_UIA_INVOKE: u32 = WM_APP + 0x3b1;
pub const WM_UIA_FOCUS: u32 = WM_APP + 0x3b2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Row,
    Text,
    Button,
    Checkbox,
}

#[derive(Clone, Debug)]
pub struct Element {
    /// Positive, stable, window-scoped identity; never reassign to another entity.
    pub id: i32,
    /// Already-sanitized accessible_name; rows supply their FULL spec §12 name.
    pub name: String,
    pub kind: Kind,
    /// Physical screen [left, top, width, height], NOT DIPs or right/bottom.
    pub bounds: [f64; 4],
    pub enabled: bool,
    pub action: Option<usize>,
    pub focusable: bool,
    pub focused: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub name: String,
    pub bounds: [f64; 4],
    /// Reading/navigation order. Later entries win overlapping hit tests, so
    /// inline/menu buttons must follow their containing row in this list.
    pub elements: Vec<Element>,
}

struct Cache {
    hwnd: isize,
    alive: bool,
    epoch: usize,
    snapshot: Snapshot,
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
        snapshot: Snapshot {
            name: String::new(),
            bounds: [0.0; 4],
            elements: Vec::new(),
        },
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
        validate(&snapshot)?;
        let mut cache = self.root.cache.lock().map_err(|_| unavailable())?;
        if !cache.alive {
            return Err(unavailable());
        }
        if epoch <= cache.epoch || epoch > isize::MAX as usize {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        cache.snapshot = snapshot;
        cache.epoch = epoch;
        Ok(())
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
    cache.snapshot.name.clear();
    cache.snapshot.name.shrink_to_fit();
    cache.snapshot.elements.clear();
    cache.snapshot.elements.shrink_to_fit();
    cache.snapshot.bounds = [0.0; 4];
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

fn string_variant(value: &str) -> Result<VARIANT> {
    if value.is_empty() {
        return Ok(VARIANT::from(BSTR::new()));
    }
    let wide: Vec<u16> = value.encode_utf16().collect();
    // Guard the installed SysAllocStringLen slice wrapper's u32 conversion.
    u32::try_from(wide.len()).map_err(|_| Error::from_hresult(E_INVALIDARG))?;
    // BSTR::from(&str) panics on native allocation failure in this version.
    let bstr = unsafe { SysAllocStringLen(Some(&wide)) };
    if bstr.is_empty() {
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }
    Ok(VARIANT::from(bstr)) // Moves BSTR ownership; VARIANT Drop clears it.
}

#[derive(Clone, Copy)]
struct Ticket {
    id: i32,
    epoch: usize,
    action: Option<usize>,
}

// Each COM interface is an address of one of these four vtable-reference slots.
// repr(C) + offset_of (not a guessed layout) recover the Arc payload from `this`.
// The object stores no HWND wrapper, thread-affine UI state, COM host, child list,
// credentials or callbacks. All slots/vtables are immutable. Arc handles lifetime.
#[repr(C)]
struct Node {
    simple: &'static IRawElementProviderSimple_Vtbl,
    fragment: &'static IRawElementProviderFragment_Vtbl,
    fragment_root: &'static IRawElementProviderFragmentRoot_Vtbl,
    invoke: &'static IInvokeProvider_Vtbl,
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
                NavigateDirection_Parent => return Ok(Some(self.root_arc())),
                NavigateDirection_NextSibling => elements.get(index + 1),
                NavigateDirection_PreviousSibling => {
                    index.checked_sub(1).and_then(|i| elements.get(i))
                }
                NavigateDirection_FirstChild | NavigateDirection_LastChild => None,
                _ => return Err(Error::from_hresult(E_INVALIDARG)),
            };
            Ok(target.map(|e| self.child(e, cache.epoch)))
        } else {
            let target = match direction {
                NavigateDirection_FirstChild => elements.first(),
                NavigateDirection_LastChild => elements.last(),
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
        let actionable = element.is_some_and(|e| e.kind != Kind::Text && e.action.is_some());
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
            }),
            UIA_AutomationIdPropertyId => string_variant(
                match element {
                    None => "quota-baton.flyout".to_owned(),
                    Some(e) => format!("quota-baton.element.{}", e.id),
                }
                .as_str(),
            )?,
            UIA_FrameworkIdPropertyId => string_variant("Win32")?,
            UIA_IsEnabledPropertyId => VARIANT::from(element.is_none_or(|e| e.enabled)),
            UIA_IsKeyboardFocusablePropertyId => {
                VARIANT::from(element.is_some_and(|e| e.focusable))
            }
            UIA_HasKeyboardFocusPropertyId => VARIANT::from(element.is_some_and(|e| e.focused)),
            UIA_IsControlElementPropertyId | UIA_IsContentElementPropertyId => VARIANT::from(true),
            UIA_IsInvokePatternAvailablePropertyId => VARIANT::from(actionable),
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
        let (index, epoch) = self.action_ticket(&cache)?;
        // Keep the short cache guard through PostMessageW: deactivate/publish
        // cannot invalidate HWND/epoch between validation and enqueueing.
        // UI MUST still compare epoch before resolving the frozen target index.
        unsafe {
            PostMessageW(
                Some(HWND(cache.hwnd as *mut c_void)),
                WM_UIA_INVOKE,
                WPARAM(index),
                LPARAM(epoch as isize),
            )
        }
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
        unsafe {
            PostMessageW(
                Some(HWND(cache.hwnd as *mut c_void)),
                WM_UIA_FOCUS,
                WPARAM(e.id as usize),
                LPARAM(cache.epoch as isize),
            )
        }
    }
}

// Raw ABI plumbing is necessary for S_OK + explicitly initialized null outputs
// (no sibling/focus/pattern/host). Generated Result<Interface> trait shims cannot
// express this without leaving the output untouched. No fabricated interfaces.

const OFFSETS: [usize; 4] = [
    offset_of!(Node, simple),
    offset_of!(Node, fragment),
    offset_of!(Node, fragment_root),
    offset_of!(Node, invoke),
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
    let slot = if iid == IUnknown::IID || iid == IRawElementProviderSimple::IID {
        0
    } else if iid == IRawElementProviderFragment::IID {
        1
    } else if iid == IRawElementProviderFragmentRoot::IID && n.ticket.is_none() {
        2
    } else if iid == IInvokeProvider::IID && n.ticket.is_some_and(|t| t.action.is_some()) {
        3
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
    let n = unsafe { node::<0>(this) };
    let result = (|| {
        let cache = n.read()?;
        let actionable = n
            .element(&cache)?
            .is_some_and(|e| e.kind != Kind::Text && e.action.is_some());
        if id != UIA_InvokePatternId || !actionable {
            return Ok(None);
        }
        // Preserve this COM object's identity when returning its Invoke slot.
        unsafe {
            Arc::increment_strong_count(n as *const Node);
        }
        Ok(Some(unsafe { Arc::from_raw(n as *const Node) }))
    })();
    unsafe { interface_output(out, result, 3) }
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
    let result: Result<Option<IRawElementProviderSimple>> = (|| {
        let cache = n.read()?;
        if n.ticket.is_some() {
            return Ok(None);
        }
        // Required UIA host bridge only; no SendMessage or app callbacks.
        // Keep teardown from recycling this HWND during the host lookup.
        Ok(Some(unsafe {
            UiaHostProviderFromHwnd(HWND(cache.hwnd as *mut c_void))?
        }))
    })();
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::SafeArrayGetElement;
    use windows::core::BSTR;

    fn scene() -> Snapshot {
        Snapshot {
            name: "계정 사용량".into(),
            bounds: [100.0, 200.0, 360.0, 400.0],
            elements: vec![
                Element {
                    id: 1,
                    name: "요약 전체 문장".into(),
                    kind: Kind::Text,
                    bounds: [110.0, 210.0, 340.0, 30.0],
                    enabled: true,
                    action: None,
                    focusable: false,
                    focused: false,
                },
                Element {
                    id: 2,
                    name: "계정, Pro, 사용 가능, 5시간 38% 남음, 2시간 초기화".into(),
                    kind: Kind::Row,
                    bounds: [104.0, 250.0, 352.0, 70.0],
                    enabled: true,
                    action: Some(4),
                    focusable: true,
                    focused: true,
                },
                Element {
                    id: 3,
                    name: "사유 보기".into(),
                    kind: Kind::Button,
                    bounds: [400.0, 260.0, 40.0, 32.0],
                    enabled: true,
                    action: Some(5),
                    focusable: true,
                    focused: false,
                },
            ],
        }
    }

    // All tests authored UNRUN. Fake HWND integer is NEVER passed to a Win32
    // window API: tests call only cache operations, raw COM, and Ole allocators.
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
