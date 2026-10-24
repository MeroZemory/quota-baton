#[cfg(all(feature = "testkit", debug_assertions))]
use super::LoginError;
use crate::accounts::Provider;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

pub(super) struct Parameters {
    pub provider: Provider,
    pub authorize: String,
    pub token: String,
    pub profile: String,
    pub client_id: &'static str,
    pub scope: &'static str,
    pub path: String,
    pub port: u16,
    manual_redirect: Option<&'static str>,
}
impl Parameters {
    pub fn new(provider: Provider) -> Self {
        let (authorize, token, client_id, scope, path, port) = match provider {
            Provider::Claude => (
                "https://claude.ai/oauth/authorize",
                "https://platform.claude.com/v1/oauth/token",
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
                "/callback",
                54545,
            ),
            Provider::Codex => (
                "https://auth.openai.com/oauth/authorize",
                "https://auth.openai.com/oauth/token",
                "app_EMoamEEZ73f0CkXaXp7hrann",
                "openid email profile offline_access",
                "/auth/callback",
                1455,
            ),
        };
        Self {
            provider,
            authorize: authorize.into(),
            token: token.into(),
            profile: "https://api.anthropic.com/api/oauth/profile".into(),
            client_id,
            scope,
            path: path.into(),
            port,
            manual_redirect: None,
        }
    }
    pub fn redirect(&self) -> String {
        if let Some(redirect) = self.manual_redirect {
            return redirect.to_owned();
        }
        format!("http://localhost:{}{}", self.port, self.path)
    }
    pub fn use_manual_redirect(&mut self) {
        self.manual_redirect = match self.provider {
            Provider::Claude => Some("https://platform.claude.com/oauth/code/callback"),
            Provider::Codex => None,
        };
    }
    pub fn auth_url(&self, state: &str, verifier: &str) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse(&self.authorize).expect("validated OAuth URL");
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", self.client_id),
            ("redirect_uri", &self.redirect()),
            ("scope", self.scope),
            ("state", state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        match self.provider {
            Provider::Claude => {
                url.query_pairs_mut().append_pair("code", "true");
            }
            Provider::Codex => {
                url.query_pairs_mut().extend_pairs([
                    ("prompt", "login"),
                    ("id_token_add_organizations", "true"),
                    ("codex_cli_simplified_flow", "true"),
                ]);
            }
        }
        url.into()
    }
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn apply_test(&mut self, test: &super::LoginTestConfig) -> Result<(), LoginError> {
        for endpoint in [&test.authorize_url, &test.token_url, &test.profile_url] {
            let url = reqwest::Url::parse(endpoint).map_err(|_| LoginError::InvalidTestEndpoint)?;
            let host = url.host_str().ok_or(LoginError::InvalidTestEndpoint)?;
            let ip: std::net::IpAddr = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse()
                .map_err(|_| LoginError::InvalidTestEndpoint)?;
            if !ip.is_loopback()
                || url.scheme() != "http"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || matches!(url.port_or_known_default(), Some(10100 | 10101))
            {
                return Err(LoginError::InvalidTestEndpoint);
            }
        }
        if matches!(test.callback_port, 10100 | 10101) {
            return Err(LoginError::InvalidTestEndpoint);
        }
        self.authorize = test.authorize_url.clone();
        self.token = test.token_url.clone();
        self.profile = test.profile_url.clone();
        self.port = test.callback_port;
        Ok(())
    }
}

pub(super) fn random_secret() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_redirects_and_authorize_parameters_are_fixed() {
        for (provider, port, redirect) in [
            (Provider::Claude, 54545, "http://localhost:54545/callback"),
            (Provider::Codex, 1455, "http://localhost:1455/auth/callback"),
        ] {
            let params = Parameters::new(provider);
            assert_eq!(params.port, port);
            assert_eq!(params.redirect(), redirect);
            let url =
                reqwest::Url::parse(&params.auth_url("synthetic-state", "synthetic-verifier"))
                    .unwrap();
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "client_id" && v == params.client_id)
            );
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "scope" && v == params.scope)
            );
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "redirect_uri" && v == redirect)
            );
            let extra = match provider {
                Provider::Claude => "code",
                Provider::Codex => "codex_cli_simplified_flow",
            };
            assert!(url.query_pairs().any(|(k, v)| k == extra && v == "true"));
        }
    }
}
