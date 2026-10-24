pub(crate) fn ipv6_unavailable(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    // Windows WSAEAFNOSUPPORT may map to Uncategorized.
    if error.raw_os_error() == Some(10047) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognizes_winsock_address_family_unavailable() {
        assert!(super::ipv6_unavailable(&std::io::Error::from_raw_os_error(
            10047
        )));
    }
}
