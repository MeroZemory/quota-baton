//! Only debug+testkit builds with both shell/clipboard actions disabled may
//! display the exact synthetic core's authorization URL. No provider URL override.
use super::LoginMethod;

pub(super) fn allowed(provider: &str, method: LoginMethod, value: &str) -> bool {
    let Ok(base) = std::env::var("QB_TEST_LOGIN_BASE") else {
        return false;
    };
    permits(
        &base,
        provider,
        method,
        value,
        std::env::var("QB_TRAY_TEST_NO_BROWSER").as_deref() == Ok("1"),
        std::env::var("QB_TRAY_TEST_NO_CLIPBOARD").as_deref() == Ok("1"),
    )
}
fn permits(
    base: &str,
    provider: &str,
    method: LoginMethod,
    value: &str,
    no_browser: bool,
    no_clipboard: bool,
) -> bool {
    if !no_browser
        || !no_clipboard
        || method != LoginMethod::Web
        || !matches!(provider, "codex" | "claude")
    {
        return false;
    }
    let (Ok(base), Ok(url)) = (reqwest::Url::parse(base), reqwest::Url::parse(value)) else {
        return false;
    };
    let Ok(ip) = base
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
    else {
        return false;
    };
    base.scheme() == "http"
        && ip.is_loopback()
        && base
            .port()
            .is_some_and(|p| p != 0 && p != 10100 && p != 10101)
        && base.username().is_empty()
        && base.password().is_none()
        && base.path() == "/"
        && base.query().is_none()
        && base.fragment().is_none()
        && url.origin() == base.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/authorize"
        && url.fragment().is_none()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r261_fixture_urls_need_all_isolation_controls_and_exact_numeric_origin() {
        let base = "http://127.0.0.1:32123";
        let url = "http://127.0.0.1:32123/authorize?state=synthetic";
        assert!(permits(base, "codex", LoginMethod::Web, url, true, true));
        for (browser, clipboard) in [(false, true), (true, false), (false, false)] {
            assert!(!permits(
                base,
                "codex",
                LoginMethod::Web,
                url,
                browser,
                clipboard
            ));
        }
        for bad in [
            "http://localhost:32123",
            "https://127.0.0.1:32123",
            "http://127.0.0.1:10100",
            "http://127.0.0.1:10101",
            "http://192.0.2.1:32123",
            "http://127.0.0.1:32123/path",
            "http://user@127.0.0.1:32123",
        ] {
            assert!(!permits(bad, "codex", LoginMethod::Web, url, true, true));
        }
        for bad in [
            "http://127.0.0.1:32124/authorize",
            "http://127.0.0.1:32123/callback",
            "http://user@127.0.0.1:32123/authorize",
            "http://127.0.0.1:32123/authorize#x",
        ] {
            assert!(!permits(base, "codex", LoginMethod::Web, bad, true, true));
        }
        assert!(!permits(
            base,
            "codex",
            LoginMethod::Device,
            url,
            true,
            true
        ));
    }
}
