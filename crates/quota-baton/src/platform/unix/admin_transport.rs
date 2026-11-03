use std::{io, os::unix::fs::FileTypeExt, path::Path};

pub(crate) fn admin_socket_client(
    builder: reqwest::ClientBuilder,
    port: u16,
) -> anyhow::Result<(reqwest::Client, bool)> {
    // A custom TCP endpoint must not silently target the managed default core.
    if port != qb_core::config::Config::default().server.admin_port {
        return Ok((builder.build()?, false));
    }
    match qb_core::service::admin_socket_path_if_allowed() {
        Some(path) => client_at(builder, port, &path),
        None => Ok((builder.build()?, false)),
    }
}

fn client_at(
    builder: reqwest::ClientBuilder,
    port: u16,
    path: &Path,
) -> anyhow::Result<(reqwest::Client, bool)> {
    if port != qb_core::config::Config::default().server.admin_port {
        return Ok((builder.build()?, false));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_socket(),
                "admin endpoint is not a socket"
            );
            Ok((builder.unix_socket(path).build()?, true))
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok((builder.build()?, false))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn native_endpoint_selection_preserves_custom_tcp_and_refuses_aliases() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("admin.sock");
        let port = qb_core::config::Config::default().server.admin_port;
        assert!(
            !client_at(reqwest::Client::builder(), port, &path)
                .unwrap()
                .1
        );
        let listener = UnixListener::bind(&path).unwrap();
        assert!(
            client_at(reqwest::Client::builder(), port, &path)
                .unwrap()
                .1
        );
        assert!(
            !client_at(reqwest::Client::builder(), 12345, &path)
                .unwrap()
                .1
        );
        let alias = home.path().join("alias.sock");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(client_at(reqwest::Client::builder(), port, &alias).is_err());
        drop(listener);
    }

    #[test]
    fn preflight_diagnostic_survives_cli_redaction_without_secret_contents() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, b"synthetic-secret-must-not-appear").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        let error = qb_core::config::Config::load(root.path())
            .unwrap_err()
            .context("safe wrapper");
        let message = crate::friendly_error(&error);
        assert!(message.contains(path.to_str().unwrap()));
        assert!(message.contains("0664"));
        assert!(message.contains("install -m 0600"));
        assert!(!message.contains("synthetic-secret"));
    }
}
