//! Windows native client-source admin entrypoint.
use super::*;

impl RelayState {
    pub(crate) async fn takeover_account(
        &self,
        request: crate::accounts::takeover::TakeoverRequest,
    ) -> Result<serde_json::Value, crate::accounts::takeover::TakeoverError> {
        if self.is_draining() {
            return Err(crate::accounts::takeover::TakeoverError::Unavailable);
        }
        let dependencies = crate::accounts::takeover_native::production(self.client.clone());
        #[cfg(all(feature = "testkit", debug_assertions))]
        let dependencies = self
            .tokens
            .takeover_context
            .dependencies()
            .cloned()
            .unwrap_or(dependencies);
        self.tokens
            .takeover(
                self.store.clone(),
                self.observability.clone(),
                request,
                dependencies,
            )
            .await
    }
}
