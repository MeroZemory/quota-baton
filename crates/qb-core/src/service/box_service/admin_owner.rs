//! Owned box-admin mutations and terminal native cleanup.
use super::{BoxService, protected_factory, unavailable};
use crate::{
    admin::boxes::{BoxAdminError, BoxAdminService, box_admin_routes, prepare_add_response},
    boxes::{BoxAuthority, BoxName, BoxRegistry, BoxScope, BoxSummary, RegistryError},
    config::Config,
    issue_operation::IssueOperation,
    relay::RelayState,
};
use axum::{Router, response::Response};
use std::{
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

const QUEUE: usize = 33; // One permit stays reserved for Stop.
const RETRY: Duration = Duration::from_millis(250);

enum Command {
    Add(
        BoxName,
        IssueOperation,
        oneshot::Sender<Result<Ready, BoxAdminError>>,
    ),
    Revoke(
        BoxName,
        IssueOperation,
        oneshot::Sender<Result<BoxScope, BoxAdminError>>,
    ),
    List(oneshot::Sender<Result<Vec<BoxSummary>, BoxAdminError>>),
    Stop,
}

struct Ready {
    scope: BoxScope,
    response: Response,
    commit_ack: oneshot::Sender<()>,
}
impl Ready {
    // No await between actual registry publication gate and returning response.
    fn publish(
        self,
        handle: &AdminHandle,
        operation: &IssueOperation,
    ) -> Result<Response, BoxAdminError> {
        let Self {
            scope,
            response,
            commit_ack,
        } = self;
        let published = handle
            .registry
            .try_with_active_scope(&scope, || {
                if handle.closed.load(Ordering::Acquire) || !operation.can_reply() {
                    return false;
                }
                commit_ack.send(()).is_ok()
            })
            .map_err(BoxAdminError::from)?;
        if published {
            Ok(response)
        } else {
            Err(BoxAdminError::Unavailable)
        }
    }
}

#[derive(Clone)]
struct AdminHandle {
    sender: mpsc::Sender<Command>,
    registry: BoxRegistry,
    closed: Arc<AtomicBool>,
}
impl AdminHandle {
    fn send(&self, command: Command) -> Result<(), BoxAdminError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(BoxAdminError::Unavailable);
        }
        self.sender
            .try_send(command)
            .map_err(|_| BoxAdminError::Unavailable)
    }
}
impl BoxAdminService for AdminHandle {
    async fn list(&self) -> Result<Vec<BoxSummary>, BoxAdminError> {
        let (send, receive) = oneshot::channel();
        self.send(Command::List(send))?;
        receive.await.map_err(|_| BoxAdminError::Unavailable)?
    }
    async fn add_for_operation(
        &self,
        name: BoxName,
        operation: IssueOperation,
    ) -> Result<Response, BoxAdminError> {
        let (send, receive) = oneshot::channel();
        self.send(Command::Add(name, operation.clone(), send))?;
        let ready = receive.await.map_err(|_| BoxAdminError::Unavailable)??;
        ready.publish(self, &operation)
    }
    async fn revoke_for_operation(
        &self,
        name: BoxName,
        operation: IssueOperation,
    ) -> Result<BoxScope, BoxAdminError> {
        let (send, receive) = oneshot::channel();
        self.send(Command::Revoke(name, operation, send))?;
        receive.await.map_err(|_| BoxAdminError::Unavailable)?
    }
}

struct Retirement {
    authority: BoxAuthority,
    native: bool,
    close_attempted: bool,
    close_failed: bool,
}
struct Returned {
    service: BoxService,
    pending: Vec<Retirement>,
}

/// Keep this owner through every error, borrowed wait and P2 durable cleanup.
#[must_use = "retain, drain and terminally join before freeze/RuntimeDrop"]
pub struct BoxServiceOwner {
    service: Option<BoxService>,
    worker: Option<JoinHandle<Returned>>,
    receiver: Option<mpsc::Receiver<Command>>,
    stop: Option<mpsc::OwnedPermit<Command>>,
    admin: Option<AdminHandle>,
    pending: Vec<Retirement>,
    failed: bool,
    drain: watch::Sender<bool>,
    relay: Weak<RelayState>,
}
impl BoxServiceOwner {
    pub fn prepare(home: &Path, config: &Config) -> anyhow::Result<Self> {
        let service = BoxService::prepare(home, config)?;
        let (admin, receiver, stop) = match service.registry.as_ref() {
            Some(registry) => {
                let (sender, receiver) = mpsc::channel(QUEUE);
                let stop = sender
                    .clone()
                    .try_reserve_owned()
                    .map_err(|_| unavailable())?;
                let admin = AdminHandle {
                    sender,
                    registry: registry.clone(),
                    closed: Arc::new(AtomicBool::new(true)),
                };
                (Some(admin), Some(receiver), Some(stop))
            }
            None => (None, None, None), // Existing empty Windows/default path.
        };
        let (drain, _) = watch::channel(false);
        Ok(Self {
            service: Some(service),
            worker: None,
            receiver,
            stop,
            admin,
            pending: Vec::new(),
            failed: false,
            drain,
            relay: Weak::new(),
        })
    }
    /// Merge only INSIDE the existing host-admin safety layer.
    pub fn admin_routes(&self, relay: Arc<RelayState>, port: u16) -> Router {
        match &self.admin {
            Some(admin) => box_admin_routes(admin.clone()).merge(
                crate::admin::boxes::scoped_launch_routes(admin.registry.clone(), relay, port),
            ),
            None => Router::new(), // No configured feature or registry/backend.
        }
    }
    /// Keep the private registry inside core while HTTP owns real accepted IO.
    /// Existing public HTTP callers retain their original no-registry surface.
    pub async fn spawn_http(
        &self,
        config: &Config,
        secrets: crate::local_secrets::LocalSecrets,
        data_routes: Router,
        extra_admin: Router,
    ) -> anyhow::Result<crate::http::RunningServers> {
        crate::http::spawn_with_box_registry(
            config,
            secrets,
            data_routes,
            extra_admin,
            self.admin.as_ref().map(|admin| admin.registry.clone()),
        )
        .await
    }
    pub fn start(&mut self, relay: Arc<RelayState>) -> anyhow::Result<()> {
        // Retain notification access even if native startup partially fails.
        self.relay = Arc::downgrade(&relay);
        let service = self.service.as_mut().ok_or_else(unavailable)?;
        // Partial native failure remains in self.service. Never move it on Err.
        if let Err(error) = service.start(relay.clone()) {
            self.failed = true;
            return Err(error);
        }
        let Some(admin) = self.admin.as_ref() else {
            return Ok(());
        };
        let bound = match service.listener_scopes() {
            Ok(bound) => bound,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        let receiver = self.receiver.take().ok_or_else(unavailable)?;
        let service = self.service.take().ok_or_else(unavailable)?;
        let worker = Worker {
            service,
            registry: admin.registry.clone(),
            relay,
            closed: admin.closed.clone(),
            bound,
            pending: Vec::new(),
            drain: self.drain.subscribe(),
        };
        self.worker = Some(crate::boxes::spawn(worker.run(receiver)));
        admin.closed.store(false, Ordering::Release);
        Ok(())
    }
    pub fn begin_drain(&mut self) {
        if let Some(admin) = &self.admin {
            admin.closed.store(true, Ordering::Release);
            if let Some(relay) = self.relay.upgrade() {
                relay.notify_box_service_stop();
            }
        }
        self.drain.send_replace(true);
        if let Some(stop) = self.stop.take() {
            stop.send(Command::Stop);
        }
        if let Some(service) = self.service.as_mut() {
            service.begin_drain();
        }
    }
    pub async fn join(&mut self) -> anyhow::Result<()> {
        self.begin_drain();
        if let Some(worker) = self.worker.as_mut() {
            // Borrow the real handle; cancelling THIS wait never cancels worker.
            let returned = worker.await;
            self.worker.take(); // Only after terminal completion.
            match returned {
                Ok(returned) => {
                    self.pending = returned.pending;
                    self.service = Some(returned.service);
                }
                Err(_) => {
                    self.failed = true;
                    return Err(unavailable());
                }
            }
        }
        let service = self.service.as_mut().ok_or_else(unavailable)?;
        // Also covers partial startup with no worker and idempotent retry.
        let result = service.join().await;
        if self.failed || !self.pending.is_empty() {
            Err(unavailable())
        } else {
            result
        }
    }
    pub fn freeze(&mut self) -> anyhow::Result<()> {
        if self.worker.is_some() || self.failed || !self.pending.is_empty() {
            return Err(unavailable());
        }
        self.service.as_mut().ok_or_else(unavailable)?.freeze()
    }
    pub fn check_complete(&self) -> anyhow::Result<()> {
        if self.worker.is_some() || self.failed || !self.pending.is_empty() {
            return Err(unavailable());
        }
        self.service
            .as_ref()
            .ok_or_else(unavailable)?
            .check_complete()
    }
}

struct Worker {
    service: BoxService,
    registry: BoxRegistry,
    relay: Arc<RelayState>,
    closed: Arc<AtomicBool>,
    bound: Vec<BoxScope>, // Actual startup scopes + attempted fallible creates.
    pending: Vec<Retirement>,
    drain: watch::Receiver<bool>,
}
impl Worker {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) -> Returned {
        loop {
            let command = tokio::select! {
                command = commands.recv() => command,
                changed = self.drain.changed() => {
                    self.service.begin_drain();
                    if changed.is_err() { break; }
                    continue;
                }
            };
            let Some(command) = command else {
                break;
            };
            if matches!(command, Command::Stop) {
                break;
            }
            if self.closed.load(Ordering::Acquire) {
                continue;
            } // Drops reply, no admission.
            match command {
                Command::Add(name, operation, reply) => self.add(name, operation, reply).await,
                Command::Revoke(name, operation, reply) => {
                    let result = self.revoke(name, operation).await;
                    let _ = reply.send(result);
                }
                Command::List(reply) => {
                    let registry = self.registry.clone();
                    let drain = self.drain.clone();
                    let result = watched_write(
                        None,
                        None,
                        drain,
                        move || registry.list(),
                        |_| self.service.begin_drain(),
                    )
                    .await;
                    let _ = reply.send(result);
                }
                Command::Stop => break,
            }
        }
        self.closed.store(true, Ordering::Release);
        // ALL accepted mutation/rollback handles have completed before here.
        // No abort/timeout of native parents or scoped writers.
        let _ = self.service.join().await; // Failure is sticky in actual service.
        Returned {
            service: self.service,
            pending: self.pending,
        }
    }
    fn fatal(&mut self) {
        self.service.failed = true;
        self.closed.store(true, Ordering::Release);
    }
    async fn add(
        &mut self,
        name: BoxName,
        operation: IssueOperation,
        reply: oneshot::Sender<Result<Ready, BoxAdminError>>,
    ) {
        let registry = self.registry.clone();
        let add_name = name.clone();
        let add_operation = operation.clone();
        let drain = self.drain.clone();
        let added = watched_write(
            None,
            Some(&operation),
            drain,
            move || {
                registry
                    .add_for_operation(add_name, &add_operation)
                    .map(|issued| issued.into_parts())
            },
            |_| self.service.begin_drain(),
        )
        .await;
        let (scope, key) = match added {
            Ok(issued) => issued,
            Err(error) => {
                if matches!(error, BoxAdminError::Native) {
                    self.fatal();
                }
                if matches!(error, BoxAdminError::Registry(RegistryError::Storage)) {
                    // add can fail after publication; the real undisclosed epoch
                    // is already memory-revoked/dirty and must become durable.
                    if let Ok(scope) = self.scope_for_name(&name).await {
                        self.rollback(&scope).await;
                    } else {
                        self.fatal();
                    }
                }
                let _ = reply.send(Err(error));
                return;
            }
        };
        if self.closed.load(Ordering::Acquire) {
            self.rollback(&scope).await;
            let _ = reply.send(Err(BoxAdminError::Unavailable));
            return;
        }
        let selected = self
            .service
            .declared
            .iter()
            .any(|n| n == scope.name().as_str());
        if selected {
            self.bound.push(scope.clone()); // Before any partial native creation.
            let listeners = match self.service.listener_scopes() {
                Ok(scopes) => scopes,
                Err(_) => {
                    self.rollback(&scope).await;
                    let _ = reply.send(Err(BoxAdminError::Unavailable));
                    return;
                }
            };
            let create = self.service.sockets.create(
                &listeners,
                &scope,
                protected_factory(
                    self.registry.clone(),
                    self.relay.clone(),
                    self.service.visible_port,
                ),
            );
            if create.is_err() {
                self.fatal();
                self.rollback(&scope).await;
                let _ = reply.send(Err(BoxAdminError::Native));
                return;
            }
        }
        let response = match prepare_add_response(&scope, &key) {
            Ok(response) => response,
            Err(error) => {
                self.rollback(&scope).await;
                let _ = reply.send(Err(error));
                return;
            }
        };
        drop(key); // Only the zeroizing prepared HTTP body retains raw key.
        let (commit_ack, acknowledged) = oneshot::channel();
        let delivered = reply
            .send(Ok(Ready {
                scope: scope.clone(),
                response,
                commit_ack,
            }))
            .is_ok();
        let drain = self.drain.clone();
        let committed = delivered
            && wait_ack_with_drain(acknowledged, operation.deadline(), drain, || {
                self.service.begin_drain()
            })
            .await;
        if !committed {
            self.rollback(&scope).await;
        }
    }
    async fn scope_for_name(&mut self, name: &BoxName) -> Result<BoxScope, BoxAdminError> {
        let registry = self.registry.clone();
        let name = name.clone();
        let drain = self.drain.clone();
        watched_write(
            None,
            None,
            drain,
            move || {
                registry
                    .list()?
                    .into_iter()
                    .find(|entry| entry.scope.name() == &name)
                    .map(|entry| entry.scope)
                    .ok_or(RegistryError::NotFound)
            },
            |_| self.service.begin_drain(),
        )
        .await
    }
    async fn retain_authority(&mut self, scope: &BoxScope) -> Result<usize, BoxAdminError> {
        // Read-only exact-epoch lookup; never creates/adopts a grant from metadata.
        let authority = loop {
            match self.registry.authority_for_retirement(scope) {
                Ok(authority) => break authority,
                Err(RegistryError::Synchronization) => {
                    let drain = self.drain.clone();
                    wait_with_drain(tokio::time::sleep(RETRY), drain, || {
                        self.service.begin_drain()
                    })
                    .await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let index = self.pending.len();
        self.pending.push(Retirement {
            authority,
            native: self.bound.contains(scope),
            close_attempted: false,
            close_failed: false,
        });
        Ok(index)
    }
    async fn rollback(&mut self, scope: &BoxScope) {
        match self.retain_authority(scope).await {
            Ok(index) => {
                if self.retire(index, false).await.is_err() {
                    self.fatal();
                }
            }
            Err(_) => self.fatal(),
        }
    }
    async fn revoke(
        &mut self,
        name: BoxName,
        operation: IssueOperation,
    ) -> Result<BoxScope, BoxAdminError> {
        let scope = self.scope_for_name(&name).await?;
        let index = self.retain_authority(&scope).await?; // Before revoke, including retry epochs.
        let registry = self.registry.clone();
        let writer_operation = operation.clone();
        let authority = self.pending[index].authority.clone();
        let drain = self.drain.clone();
        let result = watched_write(
            Some(&authority),
            Some(&operation),
            drain,
            move || registry.revoke_for_operation(&name, &writer_operation),
            |event| self.event(index, event),
        )
        .await;
        let durable = match result {
            Ok(actual) if actual == scope => true,
            Err(BoxAdminError::Registry(RegistryError::Storage)) => false,
            Err(error) => {
                if matches!(error, BoxAdminError::Native) {
                    self.fatal(); // JoinError: mutation outcome is not known clean.
                } else {
                    self.pending.remove(index);
                }
                return Err(error);
            }
            _ => {
                self.fatal();
                return Err(BoxAdminError::Unavailable);
            }
        };
        if self.retire(index, durable).await.is_err() {
            self.fatal();
            return Err(BoxAdminError::Native);
        }
        Ok(scope)
    }
    async fn retire(&mut self, index: usize, mut durable: bool) -> Result<(), BoxAdminError> {
        let authority = self.pending[index].authority.clone();
        let scope = authority.scope().clone();
        let native = self.pending[index].native;
        // First mandatory revoke attempt, before native close. On persistence
        // failure close IO immediately anyway, retaining originals and evidence.
        if !durable {
            durable = self.persist_revoke(index).await?;
        }
        if authority.is_revoked() {
            self.close_scope(index);
        }
        while !durable {
            let drain = self.drain.clone();
            wait_with_drain(tokio::time::sleep(RETRY), drain, || {
                self.service.begin_drain()
            })
            .await;
            durable = self.persist_revoke(index).await?;
        }
        let joined = if native {
            // A drain notification can cancel only this BORROWED wait. Actual
            // transport/parent handles remain stored in SocketSet for re-await.
            let mut draining = *self.drain.borrow();
            if draining {
                self.service.begin_drain();
            }
            loop {
                tokio::select! {
                    joined = self.service.sockets.join_scope(&scope) => break joined,
                    _ = self.drain.changed(), if !draining => {
                        draining = true;
                        self.service.begin_drain();
                    },
                }
            }
        } else {
            Ok(())
        };
        let drain = self.drain.clone();
        wait_with_drain(authority.wait_quiescent(), drain, || {
            self.service.begin_drain()
        })
        .await;
        if self.pending[index].close_failed || joined.is_err() {
            return Err(BoxAdminError::Native);
        }
        if native {
            self.service
                .sockets
                .finish_retirement(&scope)
                .map_err(|_| BoxAdminError::Native)?;
        }
        self.bound.retain(|bound| bound != &scope);
        self.pending.remove(index); // Only after durable write + terminal joins + unlink.
        Ok(())
    }
    fn event(&mut self, index: usize, event: WriteEvent) {
        match event {
            WriteEvent::GrantClosed => self.close_scope(index),
            WriteEvent::Drain => self.service.begin_drain(),
        }
    }
    fn close_scope(&mut self, index: usize) {
        let retiring = &mut self.pending[index];
        if retiring.close_attempted {
            return;
        }
        retiring.close_attempted = true;
        if retiring.native {
            retiring.close_failed = self
                .service
                .sockets
                .begin_revoke(retiring.authority.scope())
                .is_err();
        }
    }
    async fn persist_revoke(&mut self, index: usize) -> Result<bool, BoxAdminError> {
        let authority = self.pending[index].authority.clone();
        let scope = authority.scope().clone();
        let registry = self.registry.clone();
        let name = scope.name().clone();
        let drain = self.drain.clone();
        match watched_write(
            Some(&authority),
            None,
            drain,
            move || registry.revoke(&name),
            |event| self.event(index, event),
        )
        .await
        {
            Ok(actual) if actual == scope => Ok(true),
            Err(BoxAdminError::Registry(
                RegistryError::Storage | RegistryError::Synchronization,
            )) => Ok(false),
            Err(error) => Err(error),
            _ => Err(BoxAdminError::Unavailable),
        }
    }
}

enum WriteEvent {
    GrantClosed,
    Drain,
}

async fn watched_write<T: Send + 'static>(
    authority: Option<&BoxAuthority>,
    operation: Option<&IssueOperation>,
    mut drain: watch::Receiver<bool>,
    work: impl FnOnce() -> Result<T, RegistryError> + Send + 'static,
    mut event: impl FnMut(WriteEvent),
) -> Result<T, BoxAdminError> {
    // Subscribe BEFORE starting the actual durable writer. Its retained handle
    // is always joined, including after caller cancellation or service drain.
    let mut revoked = authority.map(BoxAuthority::watch_revocation);
    let mut caller = operation.map(IssueOperation::caller_guard);
    let mut draining = *drain.borrow();
    if draining {
        drop(caller.take());
        event(WriteEvent::Drain);
    }
    let mut closed = authority.is_some_and(BoxAuthority::is_revoked);
    if closed {
        event(WriteEvent::GrantClosed);
    }
    let mut writer = crate::boxes::spawn_blocking(work);
    loop {
        tokio::select! {
            result = &mut writer => {
                // Completion may win select even when the grant closed first.
                if !closed && authority.is_some_and(BoxAuthority::is_revoked) {
                    event(WriteEvent::GrantClosed);
                }
                if let Some(caller) = caller.take() { caller.disarm(); }
                return result.map_err(|_| BoxAdminError::Native)?.map_err(Into::into);
            }
            _ = async {
                match revoked.as_mut() {
                    Some(revoked) => revoked.revoked().await,
                    None => std::future::pending().await,
                }
            }, if !closed && revoked.is_some() => {
                closed = true;
                event(WriteEvent::GrantClosed);
            }
            _ = drain.changed(), if !draining => {
                draining = true;
                drop(caller.take());
                event(WriteEvent::Drain);
            }
        }
    }
}

async fn wait_with_drain<F: std::future::Future>(
    future: F,
    mut drain: watch::Receiver<bool>,
    mut on_drain: impl FnMut(),
) -> F::Output {
    tokio::pin!(future);
    let mut draining = *drain.borrow();
    if draining {
        on_drain();
    }
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = drain.changed(), if !draining => {
                draining = true;
                on_drain();
            }
        }
    }
}

async fn wait_ack_with_drain(
    acknowledged: oneshot::Receiver<()>,
    deadline: tokio::time::Instant,
    drain: watch::Receiver<bool>,
    on_drain: impl FnMut(),
) -> bool {
    wait_with_drain(wait_ack(acknowledged, deadline), drain, on_drain).await
}

async fn wait_ack(mut acknowledged: oneshot::Receiver<()>, deadline: tokio::time::Instant) -> bool {
    if matches!(
        tokio::time::timeout_at(deadline, &mut acknowledged).await,
        Ok(Ok(()))
    ) {
        return true;
    }
    // Closing linearizes against Sender::send. A successful send buffered before
    // close is still publication; a later send MUST fail in the caller's gate.
    // Never roll back an acknowledged key merely because this task polled late.
    acknowledged.close();
    acknowledged.try_recv().is_ok()
}

#[cfg(test)]
#[path = "admin_owner_tests.rs"]
mod tests;
