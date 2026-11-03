pub(crate) fn admin_socket_client(
    builder: reqwest::ClientBuilder,
    _: u16,
) -> anyhow::Result<(reqwest::Client, bool)> {
    Ok((builder.build()?, false))
}
