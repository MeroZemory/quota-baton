//! State used only by the selected CPA workflow and explicit injected testkit.
use super::{Dependencies, Journal, Owner};
use std::{io, sync::Arc};
use tokio::sync::Mutex;

#[doc(hidden)]
#[derive(Clone, Default)]
pub struct RefreshSourceContext {
    dependencies: Option<Dependencies>,
    admission: Arc<Mutex<()>>,
}

impl RefreshSourceContext {
    pub fn with_dependencies(mut self, dependencies: Dependencies) -> Self {
        self.dependencies = Some(dependencies);
        self
    }

    #[cfg(all(feature = "testkit", debug_assertions))]
    pub fn dependencies(&self) -> Option<&Dependencies> {
        self.dependencies.as_ref()
    }

    pub fn admission(&self) -> &Mutex<()> {
        &self.admission
    }

    pub fn verify_retired_source(&self, journal: &Journal) -> io::Result<()> {
        let unavailable = || io::Error::other("native takeover source unavailable");
        let deps = self.dependencies.as_ref().ok_or_else(unavailable)?;
        if deps.source.owner(journal.provider, &journal.source) != Owner::Stopped {
            return Err(unavailable());
        }
        deps.source
            .retire(
                journal.provider,
                &journal.source,
                &journal.destination,
                journal.keep_source,
            )
            .map_err(|_| unavailable())
    }
}
