//! Authenticated loopback-only, memory-only launch grant handover.
use super::*;
use axum::{body::Body, response::Response};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MAX_REPLY_BYTES: usize = crate::launch_tokens::MAX_HANDOVER_BYTES;

// No Debug/Display: grant associations are sensitive even without raw tokens.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Managed {
    schema: u16,
    binding_snapshot: serde_json::Value,
    launch_tokens: crate::launch_tokens::HandoverSnapshot,
}

struct SecretBody(Zeroizing<Vec<u8>>);
impl AsRef<[u8]> for SecretBody {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

struct BoundWriter<'a>(&'a mut Vec<u8>);
impl std::io::Write for BoundWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_REPLY_BYTES {
            return Err(std::io::Error::other("takeover reply bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RelayState {
    /// Only lifecycle_drain calls this after authenticated loopback admission.
    /// Bytes own the zeroizing allocation until the response body is dropped.
    pub(crate) fn takeover_reply(&self, bindings: serde_json::Value, requested: bool) -> Response {
        let managed = requested
            .then(|| self.launch_tokens.export_handover().ok())
            .flatten()
            .map(|launch_tokens| Managed {
                schema: 1,
                binding_snapshot: bindings.clone(),
                launch_tokens,
            });
        let mut reply = qb_proto::admin_control::LifecycleReply {
            bindings: bindings
                .get("bindings")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
            lease_released: true,
            managed,
        };
        let encode =
            |reply: &qb_proto::admin_control::LifecycleReply<serde_json::Value, Managed>| {
                let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_REPLY_BYTES));
                serde_json::to_writer(BoundWriter(&mut bytes), reply).map(|()| bytes)
            };
        let bytes = match encode(&reply) {
            Ok(bytes) => bytes,
            Err(_) => {
                // A managed grant overflow cannot block an otherwise valid
                // legacy takeover. Dropping this subtree wipes its grant rows.
                reply.managed = None;
                match encode(&reply) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return super::native_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            Provider::Codex,
                            "takeover_unavailable",
                            "Takeover reply unavailable",
                        );
                    }
                }
            }
        };
        let content_length = bytes.len();
        let mut response = Response::new(Body::from(bytes::Bytes::from_owner(SecretBody(bytes))));
        response.headers_mut().insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from(content_length),
        );
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }

    /// Startup only, before Arc publication/listeners. The caller must have
    /// obtained these bounded bytes from authenticated local lifecycle_drain.
    /// All token validation/staging precedes table mutation. A failed managed
    /// restore imports the legacy table and never publishes a partial registry.
    pub fn restore_takeover(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), crate::launch_tokens::HandoverError> {
        use crate::launch_tokens::HandoverError;
        if bytes.len() > MAX_REPLY_BYTES {
            return Err(HandoverError::Bounds);
        }
        // Read the old shape without allocating or copying the secret subtree.
        let legacy_reply: qb_proto::admin_control::LifecycleReply<
            serde_json::Value,
            serde::de::IgnoredAny,
        > = serde_json::from_slice(bytes).map_err(|_| HandoverError::Invalid)?;
        if !legacy_reply.lease_released {
            return Err(HandoverError::Invalid);
        }
        let mut legacy_bindings = legacy_reply.bindings;
        if let Some(rows) = legacy_bindings.as_array_mut() {
            // A provisional anchor is invalid without its managed metadata.
            // Refusal fallback retains real C8 bindings, never orphan authority.
            rows.retain(|row| {
                !row["key"]
                    .as_str()
                    .is_some_and(|key| key.starts_with("launch:"))
            });
        }
        let legacy = serde_json::json!({"schema":1,"bindings":legacy_bindings});
        let managed = serde_json::from_slice::<
            qb_proto::admin_control::LifecycleReply<serde_json::Value, Managed>,
        >(bytes)
        .ok()
        .and_then(|reply| reply.managed);
        if let Some(managed) = managed {
            let restored = (|| {
                if managed.schema != 1 {
                    return Err(());
                }
                let activity = managed.launch_tokens.binding_activity().map_err(|_| ())?;
                let binding_snapshot = self
                    .bindings
                    .prepare_launch_handover(&managed.binding_snapshot, &activity)
                    .map_err(|_| ())?;
                let scopes = crate::boxes::BoxRegistry::open(self.store.home())
                    .map_err(|_| ())?
                    .list()
                    .map_err(|_| ())?
                    .into_iter()
                    .map(|row| row.scope)
                    .collect::<Vec<_>>();
                let prepared = crate::launch_tokens::LaunchTokenRegistry::prepare_handover(
                    managed.launch_tokens,
                    &scopes,
                )
                .map_err(|_| ())?;
                self.bindings.import(binding_snapshot).map_err(|_| ())?;
                let registry = prepared.commit();
                registry.reattach_binding_activity(&self.bindings);
                self.launch_tokens = registry;
                Ok::<_, ()>(())
            })();
            if restored.is_ok() {
                return Ok(());
            }
            tracing::warn!(
                reason = "managed_restore_refused",
                "Launch handover unavailable; using legacy recovery"
            );
        }
        self.bindings
            .import(legacy)
            .map_err(|_| HandoverError::Invalid)
    }
}
