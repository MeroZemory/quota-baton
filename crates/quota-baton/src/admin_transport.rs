//! Shared CLI admin transport. Native sockets do not read the private bearer file.
use std::{path::Path, time::Duration};

pub(crate) async fn connection(
    home: &Path,
    config: &qb_core::config::Config,
    timeout: Duration,
    require_token: bool,
) -> anyhow::Result<(reqwest::Client, String, String, bool)> {
    let builder = || {
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout);
        if require_token {
            builder
        } else {
            builder.user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
        }
    };
    let base = format!("http://127.0.0.1:{}/admin/v1", config.server.admin_port);
    let (mut client, mut native_socket) =
        crate::platform::admin_socket_client(builder(), config.server.admin_port)?;
    if native_socket {
        // Only an idempotent health probe is retried over TCP. No mutation has
        // been submitted, so a stale optional listener cannot duplicate writes.
        let ready = client
            .get(format!("{base}/health"))
            .timeout(timeout.min(Duration::from_secs(2)))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if !ready {
            client = builder().build()?;
            native_socket = false;
        }
    }
    let token = if native_socket {
        String::new()
    } else if require_token {
        qb_core::local_secrets::load_existing(home)?.admin_token
    } else {
        std::fs::read_to_string(home.join("admin.token")).unwrap_or_default()
    };
    Ok((client, base, token, native_socket))
}
