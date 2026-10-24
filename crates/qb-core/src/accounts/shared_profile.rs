//! OAuth profile resolution shared independently of native credential sources.
use super::Identity;
use super::takeover::{AsyncResult, ProfileIdentity, ProfileResolver, TakeoverError};
use std::time::Duration;

pub(crate) const PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";

pub(crate) struct SharedProfile {
    pub client: reqwest::Client,
    pub endpoint: String,
}
impl ProfileResolver for SharedProfile {
    #[cfg(any(test, feature = "testkit"))]
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn claude<'a>(&'a self, access: &'a str) -> AsyncResult<'a, ProfileIdentity> {
        Box::pin(async move {
            #[cfg(any(test, feature = "testkit"))]
            if !crate::oauth::claude_profile::synthetic_endpoint(&self.endpoint) {
                return Err(TakeoverError::Unavailable);
            }
            let mut response = self
                .client
                .get(&self.endpoint)
                .timeout(Duration::from_secs(10))
                .bearer_auth(access)
                .header(reqwest::header::ACCEPT, "application/json")
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| TakeoverError::Unavailable)?;
            if !response.status().is_success() {
                return Err(TakeoverError::Invalid);
            }
            let mut bytes = zeroize::Zeroizing::new(Vec::new());
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| TakeoverError::Unavailable)?
            {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    return Err(TakeoverError::Invalid);
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| TakeoverError::Invalid)?;
            let required = |object: &str, key: &str| {
                value[object][key]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .map(str::to_owned)
                    .ok_or(TakeoverError::Invalid)
            };
            let account_uuid = required("account", "uuid")?;
            let organization_uuid = required("organization", "uuid")?;
            let email = required("account", "email")?;
            Ok(ProfileIdentity {
                identity: Identity::Claude {
                    account_uuid,
                    organization_uuid: Some(organization_uuid),
                    email: email.clone(),
                },
                email,
                plan: crate::oauth::claude_profile::plan(&value).map(str::to_owned),
            })
        })
    }
}
