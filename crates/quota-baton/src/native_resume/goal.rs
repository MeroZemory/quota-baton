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
//! or revoked tickets are never recreated from duplicate receipts. Only a matching
//! automatic ACK and a fresh, successfully completed foreground turn can mint the
//! single-use NextEpisodeProof. After separate core Advance, the serialized parent
//! may consume it to install a fresh controller/namespace for a future distinct
//! stop. Notifications and observation.rs never supply core authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Value, json};

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_PENDING: usize = 64;
pub(crate) const MAX_RETIRED_NAMESPACES: usize = 64;
pub(crate) const MAX_SEEN_TURNS: usize = 1024;
pub(crate) const MAX_TURN_ID_BYTES: usize = 256;

/// Payloads omit the stdio delimiter. Normal forwarded payloads are byte-identical.
pub(crate) enum Action {
    ToNative(Vec<u8>),
    /// Automatic activation must pass a fresh parent authority check at write time.
    ResumeNative(Vec<u8>),
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
    ProofInvalid,
    RetiredNamespaceBound,
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
            Self::ProofInvalid => "native_goal_proof_invalid",
            Self::RetiredNamespaceBound => "native_goal_retired_namespace_bound",
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
    thread_id: String,
    namespace: String,
    generation: u64,
}

impl StoppedGoalTicket {
    /// Correlated native thread identity for the parent's exact C8-root check.
    /// This is not core authority and cannot identify a launch by itself.
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

/// Opaque, nonClone, single-use evidence from this controller's ACK and actual
/// serialized turn progress. Neither an observer event nor core reset authority.
pub(crate) struct NextEpisodeProof {
    namespace: String,
    generation: u64,
}

struct Progress {
    turn_id: String,
    completed: bool,
}

struct Pending {
    method: String,
    params: Value,
    generation: u64,
    quota_before_request: bool,
}

struct StoppedCandidate {
    goal: Value,
    generation: u64,
}

/// No callbacks, I/O, sleeps, retries or automatic rearming.
pub(crate) struct Controller {
    namespace: String,
    generation: u64,
    connected: bool,
    current: Option<String>,
    active: Option<Value>,
    dispatched_goal: Option<Value>,
    dispatched_generation: Option<u64>,
    progress: Option<Progress>,
    proof_taken: bool,
    automatic_vetoed: bool,
    seen_turns: BTreeSet<String>,
    retired_namespaces: Vec<String>,
    quota_observed: bool,
    stop_candidate: Option<StoppedCandidate>,
    stop_read: Option<StoppedCandidate>,
    stop_read_issued: bool,
    stopped: Option<Value>,
    sealed: bool,
    ticket_taken: bool,
    pending: BTreeMap<String, Pending>,
    // Confirmed native housekeeping lifetimes, not foreground authority. These
    // survive user-goal generations until their correlated cleanup completes.
    ephemeral_threads: BTreeSet<String>,
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
            dispatched_generation: None,
            progress: None,
            proof_taken: false,
            automatic_vetoed: false,
            seen_turns: BTreeSet::new(),
            retired_namespaces: Vec::new(),
            quota_observed: false,
            stop_candidate: None,
            stop_read: None,
            stop_read_issued: false,
            stopped: None,
            sealed: false,
            ticket_taken: false,
            pending: BTreeMap::new(),
            ephemeral_threads: BTreeSet::new(),
            delivery: Delivery::NotSent,
        })
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.delivery
    }

    /// Loss/revocation of the separate core authority disables automatic work
    /// without taking away the user's still-live ordinary native transport.
    /// An activation already handed to the writer remains unknown; never retry.
    pub(crate) fn cancel_resume(&mut self) {
        self.automatic_vetoed = true;
        self.progress = None;
        self.ticket_taken = true;
        self.stopped = None;
        self.stop_candidate = None;
        self.stop_read = None;
        self.active = None;
        self.quota_observed = false;
        if matches!(self.delivery, Delivery::NotSent | Delivery::Checking) {
            self.delivery = Delivery::Cancelled;
        }
    }

    pub(crate) fn disconnect(&mut self) {
        self.automatic_vetoed = true;
        self.progress = None;
        self.connected = false;
        self.stopped = None;
        self.stop_candidate = None;
        self.stop_read = None;
        self.active = None;
        self.dispatched_goal = None;
        self.quota_observed = false;
        self.pending.clear();
        if matches!(self.delivery, Delivery::NotSent | Delivery::Checking) {
            self.delivery = Delivery::Cancelled;
        }
    }

    fn revoke(&mut self) -> Result<(), Error> {
        if self.sealed {
            self.automatic_vetoed = true;
            self.progress = None;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        self.stopped = None;
        self.stop_candidate = None;
        self.stop_read = None;
        if self.sealed && matches!(self.delivery, Delivery::NotSent | Delivery::Checking) {
            self.delivery = Delivery::Cancelled;
        }
        Ok(())
    }

    fn forget_goal(&mut self) {
        self.active = None;
        self.quota_observed = false;
        self.stop_candidate = None;
        self.stop_read = None;
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
        if value["id"].as_str().is_some_and(|s| {
            s.starts_with(&self.namespace)
                || self.retired_namespaces.iter().any(|ns| s.starts_with(ns))
        }) {
            return Err(Error::NamespaceCollision);
        }
        self.remember_turn_fields(&value["params"]);
        // While learning ownership, unrelated pipelined reads must not erase a
        // correlated selection/user-goal reply. They still block sealing via
        // `pending`. After sealing EVERY UI frame vetoes the episode. Responses
        // can be user approval/denial and therefore always revoke as well.
        let method = value["method"].as_str();
        // Native title generation starts a separate ephemeral thread. It is
        // not a foreground selection; pending RPCs still prevent sealing and
        // every UI frame after sealing still revokes automatic authority.
        let auxiliary_start =
            method == Some("thread/start") && value["params"]["ephemeral"] == true;
        // Temporary title threads unsubscribe independently of the foreground.
        // Unknown/current targets remain conservative. Archive is not exempt:
        // it can remove descendants as well as the explicitly named thread.
        let unrelated_unsubscribe = method == Some("thread/unsubscribe")
            && self.current.as_deref().is_some_and(|current| {
                value["params"]["threadId"]
                    .as_str()
                    .is_some_and(|target| !target.is_empty() && target != current)
            });
        if self.sealed
            || method.is_none_or(|method| {
                owner_edit(method) && !auxiliary_start && !unrelated_unsubscribe
                    || method.starts_with("turn/")
                    || method.starts_with("thread/goal/") && method != "thread/goal/get"
            })
        {
            self.revoke()?;
        }
        if let Some(method) = method {
            // Responses veto authority but never create pending UI requests.
            if owner_edit(method) && !auxiliary_start && !unrelated_unsubscribe {
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
        if let Some((id, retired)) = value["id"].as_str().and_then(|id| {
            self.retired_namespaces
                .iter()
                .find(|ns| id.starts_with(ns.as_str()))
                .map(|ns| (id, ns))
        }) {
            // Only the three RPCs owned by a retired controller can have
            // late replies. Unknown suffixes and native requests are collisions.
            if value.get("method").is_some()
                || !matches!(id.strip_prefix(retired.as_str()), Some("1" | "2" | "3"))
            {
                return Err(Error::NamespaceCollision);
            }
            return Ok(Action::Suppress);
        }
        if let Some(id) = value["id"]
            .as_str()
            .filter(|id| id.starts_with(&self.namespace))
        {
            // Suppress ALL private responses, including late/duplicate/unknown IDs.
            if value.get("method").is_some() {
                return Err(Error::NamespaceCollision);
            }
            if id == format!("{}3", self.namespace) {
                if let Some(read) = self.stop_read.take() {
                    let goal = &value["result"]["goal"];
                    if value.get("error").is_some()
                        || read.goal != *goal
                        || read.generation != self.generation
                        || !self.sealed
                        || self.ticket_taken
                        || self.automatic_vetoed
                        || self.delivery != Delivery::NotSent
                        || !self.quota_observed
                        || !self.pending.is_empty()
                        || !self.ephemeral_threads.is_empty()
                        || goal["threadId"].as_str() != self.current.as_deref()
                        || !self.active.as_ref().is_some_and(|active| {
                            active["status"] == "active" && same_identity(active, goal)
                        })
                    {
                        self.cancel_resume();
                    } else {
                        // Ownership evidence only. The parent must still obtain
                        // core authority and perform the unchanged final read.
                        self.stopped = Some(read.goal);
                    }
                } else if !self.stop_read_issued {
                    // A receipt before issuance cannot be replayed as correlation.
                    self.cancel_resume();
                }
                return Ok(Action::Suppress);
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
                self.dispatched_generation = Some(self.generation);
                self.quota_observed = false;
                return Ok(self.inject_resume(
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
                    self.automatic_vetoed = true;
                    self.progress = None;
                } else if self.dispatched_goal.as_ref().is_some_and(|active| {
                    same_identity(active, &value["result"]["goal"])
                        && value["result"]["goal"]["status"] == "active"
                }) {
                    self.delivery = Delivery::Acknowledged;
                } else {
                    // A mismatching/malformed ACK cannot be repaired by replay.
                    self.delivery = Delivery::Rejected;
                    self.automatic_vetoed = true;
                    self.progress = None;
                }
            }
            return Ok(Action::Suppress);
        }

        if value.get("method").is_none() {
            self.remember_turn_fields(&value["result"]);
            if let Some(turns) = value["result"]["thread"]["turns"].as_array() {
                for turn in turns {
                    self.remember_turn(turn["id"].as_str());
                }
            }
            if let Some(turns) = value["result"]["turns"].as_array() {
                for turn in turns {
                    self.remember_turn(turn["id"].as_str());
                }
            }
            let key = id_key(&value["id"])?;
            if let Some(pending) = self.pending.remove(&key)
                && value.get("error").is_none()
            {
                self.observe_cleanup(&pending, &value["result"])?;
                if pending.generation == self.generation {
                    self.observe_response(pending, &value["result"])?;
                }
            }
        } else {
            let method = value["method"].as_str().ok_or(Error::InvalidFrame)?;
            let params = &value["params"];
            self.observe_turn(&value)?;
            if self
                .current
                .as_deref()
                .is_some_and(|id| params["threadId"].as_str() == Some(id))
            {
                if method == "thread/goal/updated" {
                    let goal = &params["goal"];
                    if goal["status"] == "active"
                        && self.resume_live()
                        && self
                            .dispatched_goal
                            .as_ref()
                            .is_some_and(|a| same_identity(a, goal))
                    {
                        // Native's genuine automatic echo can precede its ACK.
                        // It preserves identity but cannot acknowledge the RPC.
                    } else if self.dispatched_goal.is_some() && goal["status"] == "usageLimited" {
                        // Another stop before explicit rearm is not current live
                        // progress and cannot be laundered into a fresh episode.
                        self.revoke()?;
                        self.forget_goal();
                    } else if self.active.as_ref().is_some_and(|a| same_identity(a, goal))
                        && goal["status"] == "usageLimited"
                    {
                        if self.stopped.as_ref().is_some_and(|s| s != goal)
                            || self
                                .stop_read
                                .as_ref()
                                .is_some_and(|read| read.goal != *goal)
                        {
                            self.revoke()?;
                        }
                        self.quota_observed = true;
                        if !self.sealed
                            && value.get("id").is_none()
                            && value.get("jsonrpc").is_none_or(|v| v == "2.0")
                        {
                            self.stop_candidate = Some(StoppedCandidate {
                                goal: goal.clone(),
                                generation: self.generation,
                            });
                        }
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
                }
            }
        }
        Ok(Action::ToUi(raw.to_vec()))
    }

    // Remember native history and unrelated turn IDs too: a preexisting or
    // foreign ID cannot become a fresh automatic turn in a later episode. Bounds
    // disable automatic work without breaking ordinary native/UI forwarding.
    fn remember_turn(&mut self, id: Option<&str>) -> bool {
        let Some(id) = id else { return false };
        if id.is_empty() || id.len() > MAX_TURN_ID_BYTES {
            self.cancel_resume();
            return false;
        }
        if self.seen_turns.contains(id) {
            return false;
        }
        if self.seen_turns.len() >= MAX_SEEN_TURNS {
            self.cancel_resume();
            return false;
        }
        self.seen_turns.insert(id.to_owned())
    }

    fn remember_turn_fields(&mut self, fields: &Value) {
        self.remember_turn(fields["turn"]["id"].as_str());
        self.remember_turn(fields["turnId"].as_str());
    }

    fn resume_live(&self) -> bool {
        self.connected
            && !self.automatic_vetoed
            && matches!(
                self.delivery,
                Delivery::UnknownNoRetry | Delivery::Acknowledged
            )
            && self.dispatched_generation == Some(self.generation)
            && self.pending.is_empty()
            && self.active.as_ref().is_some_and(|active| {
                active["status"] == "active"
                    && active["threadId"].as_str() == self.current.as_deref()
                    && self
                        .dispatched_goal
                        .as_ref()
                        .is_some_and(|original| same_identity(active, original))
            })
    }

    fn observe_turn(&mut self, value: &Value) -> Result<(), Error> {
        let method = value["method"].as_str();
        let params = &value["params"];
        let id = params["turn"]["id"].as_str();
        let fresh = self.remember_turn(id);
        self.remember_turn(params["turnId"].as_str());
        let foreground = self
            .current
            .as_deref()
            .is_some_and(|thread| params["threadId"].as_str() == Some(thread));
        let notification =
            value.get("id").is_none() && value.get("jsonrpc").is_none_or(|v| v == "2.0");
        if method == Some("turn/started") && foreground {
            if self.resume_live()
                && notification
                && fresh
                && params["turn"]["status"] == "inProgress"
                && params["turn"]["error"].is_null()
                && self.progress.is_none()
            {
                self.progress = Some(Progress {
                    turn_id: id.expect("fresh validated turn id").to_owned(),
                    completed: false,
                });
            } else if self.quota_observed || self.sealed {
                self.revoke()?;
                self.quota_observed = false;
            }
        } else if method == Some("turn/completed")
            && self
                .progress
                .as_ref()
                .is_some_and(|p| Some(p.turn_id.as_str()) == id && !p.completed)
        {
            if foreground
                && notification
                && self.resume_live()
                && params["turn"]["status"] == "completed"
                && params["turn"]["error"].is_null()
            {
                if let Some(progress) = &mut self.progress {
                    progress.completed = true;
                }
            } else {
                self.revoke()?;
            }
        }
        Ok(())
    }

    /// Ok(None) is pending/already taken; a terminal veto is explicit so the
    /// parent can promptly close core control without closing ordinary stdio.
    pub(crate) fn take_next_episode_proof(&mut self) -> Result<Option<NextEpisodeProof>, Error> {
        if self.retired_namespaces.len() >= MAX_RETIRED_NAMESPACES {
            self.cancel_resume();
            return Err(Error::RetiredNamespaceBound);
        }
        if !self.connected
            || self.automatic_vetoed
            || matches!(self.delivery, Delivery::Cancelled | Delivery::Rejected)
        {
            return Err(Error::ProofInvalid);
        }
        if self.proof_taken || !self.proof_ready() {
            return Ok(None);
        }
        self.proof_taken = true;
        Ok(Some(NextEpisodeProof {
            namespace: self.namespace.clone(),
            generation: self.generation,
        }))
    }

    fn proof_ready(&self) -> bool {
        self.resume_live()
            && self.delivery == Delivery::Acknowledged
            && self.progress.as_ref().is_some_and(|p| p.completed)
    }

    /// Explicit proof consumption only; the parent must first obtain separate
    /// core Advance and atomically exclude pending ingress around this call.
    /// Never recycle a controller, ticket, namespace or private response receipt.
    pub(crate) fn rearm(
        &mut self,
        proof: NextEpisodeProof,
        namespace: String,
    ) -> Result<(), Error> {
        let result = self.rearm_checked(proof, namespace);
        if result.is_err() {
            self.cancel_resume();
        }
        result
    }

    fn rearm_checked(&mut self, proof: NextEpisodeProof, namespace: String) -> Result<(), Error> {
        if !self.proof_taken
            || !self.proof_ready()
            || proof.namespace != self.namespace
            || proof.generation != self.generation
        {
            return Err(Error::ProofInvalid);
        }
        if namespace == self.namespace || self.retired_namespaces.contains(&namespace) {
            return Err(Error::NamespaceCollision);
        }
        if self.retired_namespaces.len() >= MAX_RETIRED_NAMESPACES {
            return Err(Error::RetiredNamespaceBound);
        }
        let mut next = Self::new(namespace)?;
        next.current = self.current.take();
        next.active = self.active.take();
        next.seen_turns = std::mem::take(&mut self.seen_turns);
        next.retired_namespaces = std::mem::take(&mut self.retired_namespaces);
        next.retired_namespaces
            .push(std::mem::take(&mut self.namespace));
        *self = next;
        Ok(())
    }

    fn observe_cleanup(&mut self, pending: &Pending, result: &Value) -> Result<(), Error> {
        if pending.method == "thread/start" && pending.params["ephemeral"] == true {
            let id = result["thread"]["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or(Error::InvalidFrame)?;
            if self.ephemeral_threads.len() >= MAX_PENDING {
                return Err(Error::PendingBound);
            }
            if self.current.as_deref() == Some(id) || !self.ephemeral_threads.insert(id.to_owned())
            {
                return Err(Error::InvalidFrame);
            }
        } else if pending.method == "thread/unsubscribe"
            && matches!(
                result["status"].as_str(),
                Some("unsubscribed" | "notLoaded" | "notSubscribed")
            )
            && let Some(id) = pending.params["threadId"].as_str()
        {
            self.ephemeral_threads.remove(id);
        }
        Ok(())
    }

    fn observe_response(&mut self, pending: Pending, result: &Value) -> Result<(), Error> {
        if matches!(
            pending.method.as_str(),
            "thread/start" | "thread/resume" | "thread/fork"
        ) && !(pending.method == "thread/start" && pending.params["ephemeral"] == true)
        {
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
                && self.ephemeral_threads.is_empty()
            {
                self.stopped = Some(goal.clone());
                self.stop_candidate = None;
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

    /// One non-mutating ownership preflight, explicitly driven by the parent.
    /// Seals at issuance: every later UI frame vetoes automatic work. This is
    /// neither core authority nor the final native read; there is no retry.
    pub(crate) fn begin_stop_read(&mut self) -> Result<Option<Action>, Error> {
        self.require_connected()?;
        if self.stop_read_issued
            || self.sealed
            || self.ticket_taken
            || self.automatic_vetoed
            || self.delivery != Delivery::NotSent
            || !self.quota_observed
            || !self.pending.is_empty()
            || !self.ephemeral_threads.is_empty()
        {
            return Ok(None);
        }
        let Some(candidate) = self.stop_candidate.as_ref() else {
            return Ok(None);
        };
        if candidate.generation != self.generation
            || candidate.goal["status"] != "usageLimited"
            || candidate.goal["threadId"].as_str() != self.current.as_deref()
            || !self.active.as_ref().is_some_and(|active| {
                active["status"] == "active" && same_identity(active, &candidate.goal)
            })
        {
            return Ok(None);
        }
        self.stop_read = self.stop_candidate.take();
        self.stop_read_issued = true;
        self.sealed = true;
        Ok(Some(self.inject(
            3,
            "thread/goal/get",
            json!({ "threadId": self.current }),
        )))
    }

    pub(crate) fn stopped_ticket(&mut self) -> Option<StoppedGoalTicket> {
        if self.ticket_taken || self.delivery != Delivery::NotSent || !self.eligible() {
            return None;
        }
        self.ticket_taken = true;
        Some(StoppedGoalTicket {
            thread_id: self.current.as_ref()?.clone(),
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
            || self.current.as_deref() != Some(ticket.thread_id.as_str())
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
            && !self.automatic_vetoed
            && self.current.is_some()
            && self.stopped.is_some()
            && self.quota_observed
            && self.pending.is_empty()
            && self.ephemeral_threads.is_empty()
    }

    fn inject(&self, serial: u8, method: &str, params: Value) -> Action {
        // Value serialization is infallible; no untrusted parser errors escape.
        let value = json!({ "id": format!("{}{serial}", self.namespace), "method": method, "params": params });
        Action::ToNative(value.to_string().into_bytes())
    }

    fn inject_resume(&self, serial: u8, method: &str, params: Value) -> Action {
        let value = json!({ "id": format!("{}{serial}", self.namespace), "method": method, "params": params });
        Action::ResumeNative(value.to_string().into_bytes())
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
