//! Serialized native ownership gate, NOT reset authorization.
//!
//! The parent must separately qualify the reset/source policy through core authority
//! before calling `begin_final_read`. Provider JSON, notifications, timers and 429
//! text cannot authorize that call. This module owns no transport or configuration.
//! The parent supplies a fresh collision-resistant private namespace for this owned
//! app-server and serializes received frames and actions through ONE stdin writer.
//! Execute a returned action before dispatching the next event; if a write's outcome
//! is uncertain, disconnect. The final check/write is local ordering, not native CAS;
//! unreceived keypresses and writers outside this dispatcher cannot be arbitrated.
//!
//! One controller represents ONE authoritative stopped-goal/reset episode. It can
//! issue one ticket and attempt one final read and one active command. Even rejected
//! or revoked tickets are never recreated from duplicate receipts. To support a
//! later episode the parent must authenticate distinct real progress followed by a
//! new stop and supply a fresh namespace/controller. Never replace a controller just
//! because a notification/read repeats. Repeated daily reset support is not wired.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Value, json};

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_PENDING: usize = 64;

/// Payloads omit the stdio delimiter. Normal forwarded payloads are byte-identical.
pub(crate) enum Action {
    ToNative(Vec<u8>),
    ToUi(Vec<u8>),
    Suppress,
}

/// Fixed categories only: no JSON, objectives, IDs or parser diagnostics in errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidNamespace,
    FrameBound,
    InvalidFrame,
    InvalidId,
    NamespaceCollision,
    PendingBound,
    DuplicateId,
    Disconnected,
    TicketInvalid,
    AttemptConsumed,
    GenerationExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidNamespace => "native_goal_invalid_namespace",
            Self::FrameBound => "native_goal_frame_bound",
            Self::InvalidFrame => "native_goal_invalid_frame",
            Self::InvalidId => "native_goal_invalid_id",
            Self::NamespaceCollision => "native_goal_namespace_collision",
            Self::PendingBound => "native_goal_pending_bound",
            Self::DuplicateId => "native_goal_duplicate_id",
            Self::Disconnected => "native_goal_disconnected",
            Self::TicketInvalid => "native_goal_ticket_invalid",
            Self::AttemptConsumed => "native_goal_attempt_consumed",
            Self::GenerationExhausted => "native_goal_generation_exhausted",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Delivery {
    NotSent,
    Checking,
    Cancelled,
    /// Entered BEFORE returning the active command; there is no retry API.
    UnknownNoRetry,
    Acknowledged,
    Rejected,
}

/// Opaque, non-cloneable native ownership evidence. NOT a core authority permit.
/// Deliberately lacks Debug/serialization to avoid exposing the private namespace.
pub(crate) struct StoppedGoalTicket {
    namespace: String,
    generation: u64,
}

struct Pending {
    method: String,
    params: Value,
    generation: u64,
    quota_before_request: bool,
}

/// No callbacks, I/O, sleeps, retries or automatic rearming.
pub(crate) struct Controller {
    namespace: String,
    generation: u64,
    connected: bool,
    current: Option<String>,
    active: Option<Value>,
    dispatched_goal: Option<Value>,
    quota_observed: bool,
    stopped: Option<Value>,
    sealed: bool,
    ticket_taken: bool,
    pending: BTreeMap<String, Pending>,
    delivery: Delivery,
}

impl Controller {
    /// Parent guarantees namespace entropy and uniqueness across episodes/servers.
    /// Require at least 128 bits' worth of hex characters, with a private prefix.
    pub(crate) fn new(namespace: String) -> Result<Self, Error> {
        let nonce = namespace
            .strip_prefix("qb-goal-")
            .and_then(|s| s.strip_suffix(':'))
            .ok_or(Error::InvalidNamespace)?;
        if !(32..=64).contains(&nonce.len()) || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidNamespace);
        }
        Ok(Self {
            namespace,
            generation: 0,
            connected: true,
            current: None,
            active: None,
            dispatched_goal: None,
            quota_observed: false,
            stopped: None,
            sealed: false,
            ticket_taken: false,
            pending: BTreeMap::new(),
            delivery: Delivery::NotSent,
        })
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.delivery
    }

    pub(crate) fn disconnect(&mut self) {
        self.connected = false;
        self.stopped = None;
        self.active = None;
        self.dispatched_goal = None;
        self.quota_observed = false;
        self.pending.clear();
        if matches!(self.delivery, Delivery::NotSent | Delivery::Checking) {
            self.delivery = Delivery::Cancelled;
        }
    }

    fn revoke(&mut self) -> Result<(), Error> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        self.stopped = None;
        if self.sealed && matches!(self.delivery, Delivery::NotSent | Delivery::Checking) {
            self.delivery = Delivery::Cancelled;
        }
        Ok(())
    }

    fn forget_goal(&mut self) {
        self.active = None;
        self.quota_observed = false;
    }

    /// Malformed/colliding/over-bound input poisons the session, even if a caller
    /// mistakenly continues. The parent must close the owned transport on error.
    pub(crate) fn ui_frame(&mut self, raw: &[u8]) -> Result<Action, Error> {
        let result = self.receive_ui(raw);
        if result.is_err() {
            self.disconnect();
        }
        result
    }

    fn receive_ui(&mut self, raw: &[u8]) -> Result<Action, Error> {
        self.require_connected()?;
        let value = decode(raw)?;
        if value["id"]
            .as_str()
            .is_some_and(|s| s.starts_with(&self.namespace))
        {
            return Err(Error::NamespaceCollision);
        }
        // While learning ownership, unrelated pipelined reads must not erase a
        // correlated selection/user-goal reply. They still block sealing via
        // `pending`. After sealing EVERY UI frame vetoes the episode. Responses
        // can be user approval/denial and therefore always revoke as well.
        let method = value["method"].as_str();
        if self.sealed
            || method.is_none_or(|method| {
                owner_edit(method)
                    || method.starts_with("turn/")
                    || method.starts_with("thread/goal/") && method != "thread/goal/get"
            })
        {
            self.revoke()?;
        }
        if let Some(method) = method {
            // Responses veto authority but never create pending UI requests.
            if owner_edit(method) {
                self.current = None;
                self.forget_goal();
            } else if method.starts_with("thread/goal/") && method != "thread/goal/get" {
                self.forget_goal();
            } else if method.starts_with("turn/") {
                self.quota_observed = false;
            }
            if let Some(id) = value.get("id") {
                let key = id_key(id)?;
                if self.pending.contains_key(&key) {
                    return Err(Error::DuplicateId);
                }
                if self.pending.len() >= MAX_PENDING {
                    return Err(Error::PendingBound);
                }
                self.pending.insert(
                    key,
                    Pending {
                        method: method.to_owned(),
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                        generation: self.generation,
                        quota_before_request: self.quota_observed,
                    },
                );
            }
        }
        Ok(Action::ToNative(raw.to_vec()))
    }

    pub(crate) fn native_frame(&mut self, raw: &[u8]) -> Result<Action, Error> {
        let result = self.receive_native(raw);
        if result.is_err() {
            self.disconnect();
        }
        result
    }

    fn receive_native(&mut self, raw: &[u8]) -> Result<Action, Error> {
        self.require_connected()?;
        let value = decode(raw)?;
        if let Some(id) = value["id"]
            .as_str()
            .filter(|id| id.starts_with(&self.namespace))
        {
            // Suppress ALL private responses, including late/duplicate/unknown IDs.
            if value.get("method").is_some() {
                return Err(Error::NamespaceCollision);
            }
            if id == format!("{}1", self.namespace) && self.delivery == Delivery::Checking {
                let goal = &value["result"]["goal"];
                if value.get("error").is_some()
                    || !self.eligible()
                    || self.stopped.as_ref() != Some(goal)
                {
                    self.delivery = Delivery::Cancelled;
                    self.stopped = None;
                    return Ok(Action::Suppress);
                }
                // Consumed before handing bytes to the writer. A lost/failed write
                // or acknowledgement is unknown, never permission to send again.
                self.delivery = Delivery::UnknownNoRetry;
                self.dispatched_goal = self.active.clone();
                return Ok(self.inject(
                    2,
                    "thread/goal/set",
                    json!({
                        "threadId": self.current, "status": "active", "origin": "automatic"
                    }),
                ));
            }
            if id == format!("{}2", self.namespace) && self.delivery == Delivery::UnknownNoRetry {
                if value.get("error").is_some() {
                    self.delivery = Delivery::Rejected;
                } else if self.dispatched_goal.as_ref().is_some_and(|active| {
                    same_identity(active, &value["result"]["goal"])
                        && value["result"]["goal"]["status"] == "active"
                }) {
                    self.delivery = Delivery::Acknowledged;
                }
            }
            return Ok(Action::Suppress);
        }

        if value.get("method").is_none() {
            let key = id_key(&value["id"])?;
            if let Some(pending) = self.pending.remove(&key)
                && value.get("error").is_none()
                && pending.generation == self.generation
            {
                self.observe_response(pending, &value["result"])?;
            }
        } else {
            let method = value["method"].as_str().ok_or(Error::InvalidFrame)?;
            let params = &value["params"];
            if self
                .current
                .as_deref()
                .is_some_and(|id| params["threadId"].as_str() == Some(id))
            {
                if method == "thread/goal/updated" {
                    let goal = &params["goal"];
                    if self.active.as_ref().is_some_and(|a| same_identity(a, goal))
                        && goal["status"] == "usageLimited"
                    {
                        if self.stopped.as_ref().is_some_and(|s| s != goal) {
                            self.revoke()?;
                        }
                        self.quota_observed = true;
                    } else if goal["status"] == "active"
                        && !self.quota_observed
                        && !self.sealed
                        && (self.active.as_ref().is_some_and(|a| same_identity(a, goal))
                            || self.pending.values().any(|p| {
                                p.generation == self.generation
                                    && p.method == "thread/goal/set"
                                    && p.params["origin"] == "user"
                                    && p.params["threadId"] == goal["threadId"]
                                    && p.params["objective"] == goal["objective"]
                                    && valid_identity(goal)
                            }))
                    {
                        // A native echo may precede the user-set response. It is
                        // neither acceptance of that request nor resume authority.
                    } else {
                        self.revoke()?;
                        self.forget_goal();
                    }
                } else if matches!(
                    method,
                    "thread/goal/cleared" | "thread/archived" | "thread/unsubscribed"
                ) {
                    self.revoke()?;
                    self.forget_goal();
                    if method != "thread/goal/cleared" {
                        self.current = None;
                    }
                } else if method == "turn/started" && (self.quota_observed || self.sealed) {
                    self.revoke()?;
                    self.quota_observed = false;
                }
            }
        }
        Ok(Action::ToUi(raw.to_vec()))
    }

    fn observe_response(&mut self, pending: Pending, result: &Value) -> Result<(), Error> {
        if matches!(
            pending.method.as_str(),
            "thread/start" | "thread/resume" | "thread/fork"
        ) {
            let id = result["thread"]["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::InvalidFrame)?;
            self.current = Some(id.to_owned());
            self.forget_goal();
        }
        if !matches!(
            pending.method.as_str(),
            "thread/goal/get" | "thread/goal/set"
        ) || self.current.is_none()
            || pending.params["threadId"].as_str() != self.current.as_deref()
        {
            return Ok(());
        }
        let goal = &result["goal"];
        if pending.method == "thread/goal/set" {
            if pending.params["origin"] == "user"
                && goal["status"] == "active"
                && valid_identity(goal)
                && goal["threadId"].as_str() == self.current.as_deref()
                && pending.params["objective"].as_str() == goal["objective"].as_str()
                && matches!(pending.params["status"].as_str(), None | Some("active"))
            {
                self.active = Some(goal.clone());
                self.quota_observed = false;
            }
        } else if self.active.as_ref().is_some_and(|a| same_identity(a, goal))
            && goal["status"] == "usageLimited"
        {
            if self.quota_observed
                && pending.quota_before_request
                && !self.sealed
                && self.pending.is_empty()
            {
                self.stopped = Some(goal.clone());
                self.sealed = true;
            }
        } else if goal["status"] == "active"
            && !self.quota_observed
            && !self.sealed
            && self.active.as_ref().is_some_and(|a| same_identity(a, goal))
        {
            // A normal live-goal read preserves the observed user identity.
        } else {
            self.revoke()?;
            self.forget_goal();
        }
        Ok(())
    }

    pub(crate) fn stopped_ticket(&mut self) -> Option<StoppedGoalTicket> {
        if self.ticket_taken || self.delivery != Delivery::NotSent || !self.eligible() {
            return None;
        }
        self.ticket_taken = true;
        Some(StoppedGoalTicket {
            namespace: self.namespace.clone(),
            generation: self.generation,
        })
    }

    /// Call ONLY after separate core authority reset qualification. Consumes the
    /// ticket even if cancelled. Does not grant or implement core authorization.
    pub(crate) fn begin_final_read(&mut self, ticket: StoppedGoalTicket) -> Result<Action, Error> {
        self.require_connected()?;
        if self.delivery != Delivery::NotSent {
            return Err(Error::AttemptConsumed);
        }
        if ticket.namespace != self.namespace
            || ticket.generation != self.generation
            || !self.eligible()
        {
            self.delivery = Delivery::Cancelled;
            return Err(Error::TicketInvalid);
        }
        self.delivery = Delivery::Checking;
        Ok(self.inject(1, "thread/goal/get", json!({ "threadId": self.current })))
    }

    fn eligible(&self) -> bool {
        self.connected
            && self.current.is_some()
            && self.stopped.is_some()
            && self.quota_observed
            && self.pending.is_empty()
    }

    fn inject(&self, serial: u8, method: &str, params: Value) -> Action {
        // Value serialization is infallible; no untrusted parser errors escape.
        let value = json!({ "id": format!("{}{serial}", self.namespace), "method": method, "params": params });
        Action::ToNative(value.to_string().into_bytes())
    }

    fn require_connected(&self) -> Result<(), Error> {
        if self.connected {
            Ok(())
        } else {
            Err(Error::Disconnected)
        }
    }
}

fn owner_edit(method: &str) -> bool {
    matches!(
        method,
        "thread/start" | "thread/resume" | "thread/fork" | "thread/unsubscribe" | "thread/archive"
    )
}

fn valid_identity(goal: &Value) -> bool {
    goal["threadId"].as_str().is_some_and(|s| !s.is_empty())
        && goal["objective"].as_str().is_some_and(|s| !s.is_empty())
        && goal["createdAt"].as_i64().is_some()
}

fn same_identity(a: &Value, b: &Value) -> bool {
    valid_identity(a)
        && valid_identity(b)
        && a["threadId"] == b["threadId"]
        && a["objective"] == b["objective"]
        && a["createdAt"] == b["createdAt"]
}

fn id_key(id: &Value) -> Result<String, Error> {
    if id.is_string() || id.as_i64().is_some() || id.as_u64().is_some() {
        Ok(id.to_string()) // JSON encoding keeps integer and string IDs distinct.
    } else {
        Err(Error::InvalidId)
    }
}

fn decode(raw: &[u8]) -> Result<Value, Error> {
    if raw.is_empty() || raw.len() > MAX_FRAME_BYTES {
        return Err(Error::FrameBound);
    }
    if raw.contains(&b'\n') || raw.contains(&b'\r') {
        return Err(Error::InvalidFrame);
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| Error::InvalidFrame)?;
    if !value.is_object() {
        return Err(Error::InvalidFrame);
    }
    if let Some(id) = value.get("id") {
        id_key(id)?;
    }
    if let Some(method) = value.get("method") {
        if method.as_str().is_none_or(|s| s.is_empty())
            || value.get("params").is_some_and(|p| !p.is_object())
            || value.get("result").is_some()
            || value.get("error").is_some()
        {
            return Err(Error::InvalidFrame);
        }
    } else if value.get("id").is_none()
        || value.get("result").is_some() == value.get("error").is_some()
    {
        return Err(Error::InvalidFrame);
    }
    Ok(value)
}
