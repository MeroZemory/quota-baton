//! Owned native continuation entry. Unsupported native argv/version combinations
//! retain the original launcher. Reset permission still comes only from core.

mod control_actor;
mod core_control;
mod dispatcher;
mod endpoint;
mod framing;
mod goal;
mod ingress;
mod managed_parent;
#[cfg(all(feature = "testkit", debug_assertions))]
mod observation;
mod parent_dispatch;
mod plan;
mod socket_pump;

use std::{ffi::OsString, path::Path, time::Duration};

use rand::{TryRngCore, rngs::OsRng};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::http::HeaderValue;

#[derive(Clone, Copy, Debug)]
pub(crate) struct LaunchError;

/// The caller retains its original admission and catalog until this future and
/// all its owned children/workers have finished. No replacement issuance occurs.
pub(crate) async fn try_run(
    program: &Path,
    args: &[OsString],
    base: &str,
    client_key: &str,
    account_short: Option<&str>,
) -> Result<Option<i32>, LaunchError> {
    let remote = plan::from_issued_args(args);
    if matches!(remote, plan::Plan::ExistingPath) {
        return Ok(None);
    }
    if !program.is_absolute() {
        return Err(LaunchError);
    }
    // Capability evidence is pinned to this native version. A failed/unknown
    // version probe preserves ordinary launch instead of guessing RPC support.
    let version = crate::launcher_label::version_output(program)
        .ok()
        .and_then(|bytes| crate::launcher_label::parse_version_output(&bytes));
    if version.as_deref() != Some("0.163.0") {
        return Ok(None);
    }
    // Explicit non-content harness observation only. No release-build reader,
    // implicit destination or authority is introduced by this capture.
    #[cfg(all(feature = "testkit", debug_assertions))]
    let capture = std::env::var_os("QB_NATIVE_OBSERVATION_PATH")
        .map(observation::Capture::start)
        .transpose()
        .map_err(|_| LaunchError)?;
    #[cfg(all(feature = "testkit", debug_assertions))]
    observation::emit(observation::Event::Entry);
    let outcome = async {
        let endpoint = endpoint::Endpoint::bind().await.map_err(|_| LaunchError)?;
        const AUTH_ENV: &str = "QB_NATIVE_REMOTE_AUTH";
        let attached = remote
            .attach_remote(&endpoint.url(), AUTH_ENV)
            .map_err(|_| LaunchError)?;
        let mut nonce = [0u8; 16];
        OsRng.try_fill_bytes(&mut nonce).map_err(|_| LaunchError)?;
        let mut namespace = String::from("qb-goal-");
        for byte in nonce {
            use std::fmt::Write;
            write!(namespace, "{byte:02x}").map_err(|_| LaunchError)?;
        }
        namespace.push(':');
        let mut bearer: HeaderValue = format!("Bearer {client_key}")
            .parse()
            .map_err(|_| LaunchError)?;
        bearer.set_sensitive(true);
        // Claim the genuine issued owner before the native server can send data.
        let control = core_control::CoreControl::connect(base, bearer)
            .await
            .map_err(|_| LaunchError)?;
        let jobs = crate::launcher_label::spawn_remote_pair(
            program,
            &attached.server_args,
            &attached.tui_args,
            AUTH_ENV,
            std::ffi::OsStr::new(endpoint.token()),
            account_short,
        )
        .map_err(|_| LaunchError)?;
        #[cfg(all(feature = "testkit", debug_assertions))]
        observation::emit(observation::Event::ChildrenSpawned);
        // jobs owns both trees already: accept failure/cancellation drops it before
        // the caller can release admission/catalog. The endpoint is single-use.
        let socket = endpoint
            .accept(Instant::now() + Duration::from_secs(15))
            .await
            .map_err(|_| LaunchError)?;
        #[cfg(all(feature = "testkit", debug_assertions))]
        observation::emit(observation::Event::EndpointAccepted);
        let owner = managed_parent::ManagedParent::new(jobs, namespace, control, socket)
            .map_err(|_| LaunchError)?;
        #[cfg(all(feature = "testkit", debug_assertions))]
        observation::emit(observation::Event::ParentEntered);
        let status = owner.run().await.map_err(|_| LaunchError)?;
        use managed_parent::JobStatus;
        Ok(Some(match status {
            JobStatus::TuiExited(code) | JobStatus::BothExited { tui: code, .. } => code,
            JobStatus::ServerExited(_) | JobStatus::Stopped | JobStatus::Running => 1,
        }))
    }
    .await;
    #[cfg(all(feature = "testkit", debug_assertions))]
    if let Some(capture) = capture {
        // This fixed diagnostic is a final join/flush receipt, not authority or
        // a successful-work receipt. Cancellation/Drop never emits it.
        capture.finish().map_err(|_| LaunchError)?;
        eprintln!("qb-native-observer-finished");
    }
    outcome
}
