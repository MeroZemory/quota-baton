//! Local managed-launch control protocol. Events and timers are hints only;
//! only a successful, single-use Consume reply admits a native activation.
//! Authentication, bounded parsing and owner lifetime belong to the transport.

use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;

/// Deliberately no Debug: even a thread identifier is private session metadata.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub request_id: u64,
    pub command: Command,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Correlates the stopped native thread with the server-observed C8 root.
    Observe {
        thread_id: String,
    },
    /// Nonconsuming readiness before the final native goal read.
    Inspect {
        episode: u64,
    },
    Consume {
        episode: u64,
    },
    /// Opens one future registration after this exact consumed episode. This
    /// carries no reset evidence and grants no activation authority.
    Advance {
        episode: u64,
    },
    Cancel {
        episode: u64,
    },
}

impl Request {
    /// Shape validation is not authority. The core must still check the exact
    /// issued owner, adopted C8 root, policy, source and native reset evidence.
    pub fn valid_shape(&self) -> bool {
        self.version == VERSION
            && self.request_id != 0
            && match &self.command {
                Command::Observe { thread_id } => {
                    !thread_id.is_empty()
                        && thread_id.len() <= 256
                        && thread_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
                }
                Command::Inspect { episode }
                | Command::Consume { episode }
                | Command::Advance { episode }
                | Command::Cancel { episode } => *episode != 0,
            }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u8,
    /// None is reserved for unsolicited Due/Cancelled hints. A hint cannot be
    /// mistaken for the correlated response to a pending Consume request.
    pub request_id: Option<u64>,
    pub event: Event,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Waiting {
        episode: u64,
        reset_unix_ms: i64,
    },
    Due {
        episode: u64,
    },
    Ready {
        episode: u64,
    },
    Granted {
        episode: u64,
    },
    /// Correlates the consumed episode, never the future registration's ID.
    Advanced {
        episode: u64,
    },
    Cancelled {
        episode: u64,
    },
    Refused {
        reason: Refusal,
    },
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Invalid,
    Unavailable,
    NotDue,
    Cancelled,
}

impl Reply {
    pub fn valid_shape(&self) -> bool {
        if self.version != VERSION || self.request_id == Some(0) {
            return false;
        }
        match &self.event {
            Event::Waiting {
                episode,
                reset_unix_ms,
            } => self.request_id.is_some() && *episode != 0 && *reset_unix_ms > 0,
            Event::Ready { episode } | Event::Granted { episode } | Event::Advanced { episode } => {
                self.request_id.is_some() && *episode != 0
            }
            Event::Due { episode } | Event::Cancelled { episode } => *episode != 0,
            Event::Refused { .. } => self.request_id.is_some(),
        }
    }

    /// Check correlation at the actual activation boundary. Receiving Due or a
    /// grant for another request/episode never admits this command. The parent
    /// separately enforces its original consume-exchange deadline and one use.
    pub fn grants(&self, request_id: u64, episode: u64) -> bool {
        self.valid_shape()
            && request_id != 0
            && episode != 0
            && self.request_id == Some(request_id)
            && matches!(self.event, Event::Granted { episode: actual } if actual == episode)
    }
}
