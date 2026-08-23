// SPDX-License-Identifier: BSD-2-Clause

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque},
    fs, io,
    os::{
        fd::{AsFd, AsRawFd},
        unix::{
            fs::{MetadataExt, PermissionsExt},
            process::ExitStatusExt,
        },
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;

use crate::{
    identity::{AccountDatabase, IdentityError},
    linux::{
        NotificationRead, ProcessError, SpawnedProcess, current_identity,
        reactor::{
            Reactor, SeqPacketConnection, SeqPacketListener, SignalFd, TimerFd, monotonic_ms,
        },
        reap_exited_child,
    },
    loader::{ConfigLoader, LoadError},
    model::{ManagerScope, ResolvedIdentity, ServiceId},
    protocol::{MessageKind, Operation, Packet, ProtocolError, StatusCode},
    runtime::{
        DesiredState, ExitOutcome, ObservedState, RuntimeEffect, RuntimeEngine, RuntimeError,
        RuntimeEvent, TimerKind,
    },
};

const LISTENER_TOKEN: u64 = 1;
const SIGNAL_TOKEN: u64 = 2;
const TIMER_TOKEN: u64 = 3;
const FIRST_DYNAMIC_TOKEN: u64 = 16;
const CONTROL_TIMEOUT_MS: u64 = 30_000;
const MAX_CLIENTS: usize = 128;
const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerMode {
    System,
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownAction {
    Exit,
    Reboot,
    Poweroff,
}

#[derive(Clone, Debug)]
pub struct ManagerOptions {
    pub mode: ManagerMode,
    pub root: PathBuf,
    pub config_home: Option<PathBuf>,
    pub runtime_dir: PathBuf,
}

impl ManagerOptions {
    #[must_use]
    pub fn system(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            runtime_dir: root.join("run"),
            mode: ManagerMode::System,
            root,
            config_home: None,
        }
    }

    #[must_use]
    pub fn user(
        root: impl Into<PathBuf>,
        config_home: impl Into<PathBuf>,
        runtime_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mode: ManagerMode::User,
            root: root.into(),
            config_home: Some(config_home.into()),
            runtime_dir: runtime_dir.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error(transparent)]
    Config(#[from] LoadError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("Linux manager IO failed: {0}")]
    Io(#[from] io::Error),
    #[error("runtime operation failed: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("invalid manager options: {0}")]
    InvalidOptions(&'static str),
    #[error("invalid service target {0:?}")]
    InvalidTarget(String),
}

pub struct Manager {
    options: ManagerOptions,
    reactor: Reactor,
    listener: SeqPacketListener,
    signals: SignalFd,
    timer: TimerFd,
    engine: RuntimeEngine,
    accounts: AccountDatabase,
    owner_identity: ResolvedIdentity,
    base_environment: BTreeMap<String, String>,
    sources: HashMap<u64, Source>,
    clients: HashMap<u64, Client>,
    processes: BTreeMap<ServiceId, ProcessSlot>,
    deadlines: BinaryHeap<Deadline>,
    next_token: u64,
    next_deadline_sequence: u64,
    shutdown: Option<ShutdownAction>,
}

impl Manager {
    /// Loads configuration, creates the control interface, and prepares the
    /// Linux event loop. Construction does not start the default group.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration/accounts, unsafe runtime
    /// directories, or Linux descriptor/socket setup failure.
    pub fn new(options: ManagerOptions) -> Result<Self, ManagerError> {
        let owner_identity = current_identity()?;
        let snapshot = match options.mode {
            ManagerMode::System => ConfigLoader::load_system(&options.root)?,
            ManagerMode::User => ConfigLoader::load_user(
                &options.root,
                options
                    .config_home
                    .as_deref()
                    .ok_or(ManagerError::InvalidOptions(
                        "user manager requires config_home",
                    ))?,
                &options.runtime_dir,
                owner_identity.uid,
            )?,
        };
        let accounts = AccountDatabase::load(&options.root)?;
        let socket_directory = control_directory(&options);
        create_control_directory(&socket_directory, options.mode, owner_identity.uid)?;
        let socket_path = socket_directory.join("control.sock");
        let socket_mode = match options.mode {
            ManagerMode::System => 0o666,
            ManagerMode::User => 0o600,
        };
        let listener = SeqPacketListener::bind(
            &socket_path,
            socket_mode,
            match options.mode {
                ManagerMode::System => 0,
                ManagerMode::User => owner_identity.uid,
            },
        )?;
        let signals = SignalFd::new(&[
            libc::SIGCHLD,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGUSR1,
        ])?;
        let timer = TimerFd::new()?;
        let reactor = Reactor::new()?;
        reactor.add(listener.as_fd(), LISTENER_TOKEN, false)?;
        reactor.add(signals.as_fd(), SIGNAL_TOKEN, false)?;
        reactor.add(timer.as_fd(), TIMER_TOKEN, false)?;

        let base_environment = match options.mode {
            ManagerMode::System => BTreeMap::from([("PATH".into(), SYSTEM_PATH.into())]),
            ManagerMode::User => std::env::vars()
                .filter(|(key, _)| key != "LOOM_NOTIFY_FD")
                .collect(),
        };

        Ok(Self {
            options,
            reactor,
            listener,
            signals,
            timer,
            engine: RuntimeEngine::new(Arc::new(snapshot)),
            accounts,
            owner_identity,
            base_environment,
            sources: HashMap::new(),
            clients: HashMap::new(),
            processes: BTreeMap::new(),
            deadlines: BinaryHeap::new(),
            next_token: FIRST_DYNAMIC_TOKEN,
            next_deadline_sequence: 0,
            shutdown: None,
        })
    }

    /// Starts the default group and serves events until an ordered shutdown is
    /// complete.
    ///
    /// # Errors
    ///
    /// Returns an error for reactor, process, timer, or control-socket failures
    /// that prevent continued supervision.
    pub fn run(mut self) -> Result<ShutdownAction, ManagerError> {
        let now = monotonic_ms()?;
        let default_group = self.engine.snapshot().default_group().clone();
        let effects = self.engine.start(&default_group, now)?;
        self.apply_effects(effects)?;

        loop {
            for event in self.reactor.wait(None)? {
                let source = match event.token {
                    LISTENER_TOKEN => Some(Source::Listener),
                    SIGNAL_TOKEN => Some(Source::Signal),
                    TIMER_TOKEN => Some(Source::Timer),
                    token => self.sources.get(&token).cloned(),
                };
                let Some(source) = source else {
                    continue;
                };
                match source {
                    Source::Listener => self.accept_clients()?,
                    Source::Signal => self.handle_signals()?,
                    Source::Timer => self.handle_deadlines()?,
                    Source::Client(token) => {
                        if event.hangup {
                            self.remove_client(token);
                        } else if event.readable {
                            self.handle_client(token)?;
                        }
                    }
                    Source::Process {
                        service,
                        generation,
                    } => self.handle_process_exit(&service, generation)?,
                    Source::Notify {
                        service,
                        generation,
                        token,
                    } => self.handle_notification(&service, generation, token)?,
                }
            }
            self.complete_pending_clients();
            if let Some(action) = self.shutdown
                && self.processes.is_empty()
                && self.engine.statuses().all(|(_, status)| {
                    matches!(
                        status.observed,
                        ObservedState::Inactive | ObservedState::Failed
                    )
                })
            {
                return Ok(action);
            }
        }
    }

    fn accept_clients(&mut self) -> Result<(), ManagerError> {
        while self.clients.len() < MAX_CLIENTS {
            let Some(connection) = self.listener.accept()? else {
                break;
            };
            let credentials = connection.peer_credentials()?;
            let token = self.allocate_token();
            self.reactor.add(connection.as_fd(), token, false)?;
            self.sources.insert(token, Source::Client(token));
            self.clients.insert(
                token,
                Client {
                    connection,
                    credentials_uid: credentials.uid,
                    pending: None,
                },
            );
        }
        Ok(())
    }

    fn handle_client(&mut self, token: u64) -> Result<(), ManagerError> {
        let packet = {
            let Some(client) = self.clients.get(&token) else {
                return Ok(());
            };
            client.connection.receive()?
        };
        let Some(encoded) = packet else {
            return Ok(());
        };
        let request = match Packet::decode(&encoded) {
            Ok(request) if request.kind == MessageKind::Request => request,
            Ok(_) => {
                self.respond(
                    token,
                    0,
                    Operation::Status,
                    StatusCode::InvalidRequest,
                    b"expected request packet".to_vec(),
                );
                return Ok(());
            }
            Err(error) => {
                self.respond(
                    token,
                    0,
                    Operation::Status,
                    StatusCode::InvalidRequest,
                    error.to_string().into_bytes(),
                );
                return Ok(());
            }
        };
        if self
            .clients
            .get(&token)
            .is_some_and(|client| client.pending.is_some())
        {
            self.respond(
                token,
                request.request_id,
                request.operation,
                StatusCode::Conflict,
                b"one request is already pending".to_vec(),
            );
            return Ok(());
        }
        if is_mutating(request.operation) && !self.may_mutate(token) {
            self.respond(
                token,
                request.request_id,
                request.operation,
                StatusCode::PermissionDenied,
                b"operation requires manager owner".to_vec(),
            );
            return Ok(());
        }
        match self.dispatch(token, &request) {
            Ok(()) => Ok(()),
            Err(error @ (ManagerError::Runtime(_) | ManagerError::InvalidTarget(_))) => {
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    StatusCode::NotFound,
                    error.to_string().into_bytes(),
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn dispatch(&mut self, token: u64, request: &Packet) -> Result<(), ManagerError> {
        if matches!(
            request.operation,
            Operation::Start | Operation::Stop | Operation::Restart
        ) {
            self.dispatch_lifecycle(token, request)?;
            self.complete_pending_clients();
            return Ok(());
        }
        match request.operation {
            Operation::Status | Operation::List => {
                let payload = self.status_payload(request.payload.as_slice());
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    StatusCode::Ok,
                    payload,
                );
            }
            Operation::IsActive => {
                let service = payload_target(request)?;
                let active = self
                    .engine
                    .status(&service)
                    .is_some_and(|status| status.observed == ObservedState::Active);
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    if active {
                        StatusCode::Ok
                    } else {
                        StatusCode::ServiceFailure
                    },
                    if active { "active" } else { "inactive" }
                        .as_bytes()
                        .to_vec(),
                );
            }
            Operation::ResetFailed => {
                let service = payload_target(request)?;
                self.engine.reset_failed(&service)?;
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    StatusCode::Ok,
                    b"reset".to_vec(),
                );
            }
            Operation::Reboot | Operation::Poweroff if self.options.mode == ManagerMode::System => {
                let action = if request.operation == Operation::Reboot {
                    ShutdownAction::Reboot
                } else {
                    ShutdownAction::Poweroff
                };
                self.begin_shutdown(action)?;
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    StatusCode::Ok,
                    b"shutdown started".to_vec(),
                );
            }
            _ => self.respond(
                token,
                request.request_id,
                request.operation,
                StatusCode::InvalidRequest,
                b"operation is not implemented yet".to_vec(),
            ),
        }
        self.complete_pending_clients();
        Ok(())
    }

    fn dispatch_lifecycle(&mut self, token: u64, request: &Packet) -> Result<(), ManagerError> {
        match request.operation {
            Operation::Start => {
                let target = payload_target(request)?;
                let services = self
                    .engine
                    .snapshot()
                    .activation_services(&target)
                    .ok_or_else(|| RuntimeError::UnknownTarget(target.clone()))?;
                let effects = self.engine.start(&target, monotonic_ms()?)?;
                self.apply_effects(effects)?;
                self.set_pending(
                    token,
                    PendingRequest::start(request.request_id, request.operation, services),
                )?;
            }
            Operation::Stop => {
                let target = payload_target(request)?;
                let effects = self.engine.stop(&target)?;
                self.apply_effects(effects)?;
                let services = self
                    .engine
                    .statuses()
                    .filter(|(_, status)| status.desired == DesiredState::Inactive)
                    .map(|(service, _)| service.clone())
                    .collect();
                self.set_pending(
                    token,
                    PendingRequest::stop(request.request_id, request.operation, services),
                )?;
            }
            Operation::Restart => {
                let service = payload_target(request)?;
                let generation = self
                    .engine
                    .status(&service)
                    .ok_or_else(|| RuntimeError::UnknownTarget(service.clone()))?
                    .generation;
                let effects = self.engine.restart(&service, monotonic_ms()?)?;
                self.apply_effects(effects)?;
                self.set_pending(
                    token,
                    PendingRequest::restart(
                        request.request_id,
                        request.operation,
                        service,
                        generation,
                    ),
                )?;
            }
            _ => unreachable!("lifecycle dispatcher receives lifecycle operations"),
        }
        Ok(())
    }

    fn set_pending(&mut self, token: u64, mut pending: PendingRequest) -> io::Result<()> {
        let now = monotonic_ms()?;
        pending.deadline_ms = now.saturating_add(CONTROL_TIMEOUT_MS);
        let request_id = pending.request_id;
        let deadline = pending.deadline_ms;
        if let Some(client) = self.clients.get_mut(&token) {
            client.pending = Some(pending);
        }
        self.push_deadline(deadline, DeadlineAction::Client { token, request_id })
    }

    fn complete_pending_clients(&mut self) {
        let completed = self
            .clients
            .iter()
            .filter_map(|(token, client)| {
                let pending = client.pending.as_ref()?;
                self.pending_result(pending)
                    .map(|result| (*token, pending.request_id, pending.operation, result))
            })
            .collect::<Vec<_>>();
        for (token, request_id, operation, (status, payload)) in completed {
            if let Some(client) = self.clients.get_mut(&token) {
                client.pending = None;
            }
            self.respond(token, request_id, operation, status, payload);
        }
    }

    fn pending_result(&self, pending: &PendingRequest) -> Option<(StatusCode, Vec<u8>)> {
        match &pending.kind {
            PendingKind::Start(services) => {
                let statuses = services
                    .iter()
                    .filter_map(|service| self.engine.status(service))
                    .collect::<Vec<_>>();
                if statuses.iter().any(|status| {
                    status.observed == ObservedState::Failed && status.blocked_by.is_none()
                }) {
                    Some((StatusCode::ServiceFailure, b"service failed".to_vec()))
                } else if statuses
                    .iter()
                    .all(|status| status.observed == ObservedState::Active)
                {
                    Some((StatusCode::Ok, b"active".to_vec()))
                } else {
                    None
                }
            }
            PendingKind::Stop(services) => services
                .iter()
                .all(|service| {
                    self.engine
                        .status(service)
                        .is_none_or(|status| status.observed == ObservedState::Inactive)
                })
                .then(|| (StatusCode::Ok, b"inactive".to_vec())),
            PendingKind::Restart {
                service,
                previous_generation,
            } => self.engine.status(service).and_then(|status| {
                if status.observed == ObservedState::Failed {
                    Some((StatusCode::ServiceFailure, b"restart failed".to_vec()))
                } else if status.observed == ObservedState::Active
                    && status.generation > *previous_generation
                {
                    Some((StatusCode::Ok, b"restarted".to_vec()))
                } else {
                    None
                }
            }),
        }
    }

    fn respond(
        &mut self,
        token: u64,
        request_id: u64,
        operation: Operation,
        status: StatusCode,
        payload: Vec<u8>,
    ) {
        let packet = Packet {
            kind: MessageKind::Response,
            request_id,
            operation,
            status,
            more: false,
            payload,
        };
        let result = packet.encode().map_err(protocol_io).and_then(|encoded| {
            self.clients
                .get(&token)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "client disconnected"))?
                .connection
                .send(&encoded)
        });
        if result.is_err() {
            self.remove_client(token);
        }
    }

    fn status_payload(&self, target: &[u8]) -> Vec<u8> {
        let target = std::str::from_utf8(target).unwrap_or("").trim();
        let mut output = String::new();
        for (service, status) in self.engine.statuses() {
            if !target.is_empty() && service.as_str() != target {
                continue;
            }
            output.push_str(service.as_str());
            output.push('\t');
            output.push_str(observed_name(status.observed));
            output.push('\t');
            output.push_str(desired_name(status.desired));
            output.push('\t');
            output.push_str(&status.generation.to_string());
            if let Some(blocked) = status.blocked_by {
                output.push_str("\tblocked-by=");
                output.push_str(blocked.as_str());
            }
            output.push('\n');
        }
        output.into_bytes()
    }

    fn may_mutate(&self, token: u64) -> bool {
        let Some(client) = self.clients.get(&token) else {
            return false;
        };
        match self.options.mode {
            ManagerMode::System => client.credentials_uid == 0,
            ManagerMode::User => client.credentials_uid == self.owner_identity.uid,
        }
    }

    fn handle_signals(&mut self) -> Result<(), ManagerError> {
        while let Some(signal) = self.signals.read_signal()? {
            match signal {
                libc::SIGTERM => {
                    self.begin_shutdown(if self.options.mode == ManagerMode::System {
                        ShutdownAction::Poweroff
                    } else {
                        ShutdownAction::Exit
                    })?;
                }
                libc::SIGINT => {
                    self.begin_shutdown(if self.options.mode == ManagerMode::System {
                        ShutdownAction::Reboot
                    } else {
                        ShutdownAction::Exit
                    })?;
                }
                libc::SIGCHLD => self.collect_process_exits()?,
                _ => {}
            }
        }
        Ok(())
    }

    fn begin_shutdown(&mut self, action: ShutdownAction) -> Result<(), ManagerError> {
        if self.shutdown.is_some() {
            return Ok(());
        }
        self.shutdown = Some(action);
        let group = self.engine.snapshot().default_group().clone();
        let effects = self.engine.stop(&group)?;
        self.apply_effects(effects)
    }

    fn apply_effects(&mut self, effects: Vec<RuntimeEffect>) -> Result<(), ManagerError> {
        let mut pending = VecDeque::from(effects);
        while let Some(effect) = pending.pop_front() {
            match effect {
                RuntimeEffect::Spawn {
                    service,
                    generation,
                } => {
                    let event = self.spawn(&service, generation);
                    pending.extend(self.engine.handle(event));
                }
                RuntimeEffect::Terminate {
                    service,
                    generation,
                    force,
                } => {
                    if let Some(slot) = self.processes.get(&service)
                        && slot.generation == generation
                    {
                        slot.process.terminate(force).map_err(process_io)?;
                    }
                }
                RuntimeEffect::ArmTimer {
                    service,
                    generation,
                    kind,
                    after_ms,
                } => {
                    self.push_deadline(
                        monotonic_ms()?.saturating_add(after_ms),
                        DeadlineAction::Runtime {
                            service,
                            generation,
                            kind,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    fn spawn(&mut self, service: &ServiceId, generation: u64) -> RuntimeEvent {
        let now = monotonic_ms().unwrap_or_default();
        let definition = &self.engine.snapshot().services()[service];
        let account = match self.accounts.resolve(
            &definition.process,
            match self.options.mode {
                ManagerMode::System => ManagerScope::System,
                ManagerMode::User => ManagerScope::User,
            },
            &self.owner_identity,
        ) {
            Ok(account) => account,
            Err(error) => {
                eprintln!("loom: {service}: {error}");
                return RuntimeEvent::SpawnFailed {
                    service: service.clone(),
                    generation,
                    at_ms: now,
                };
            }
        };
        let mut environment = self.base_environment.clone();
        environment.insert("HOME".into(), account.home);
        environment.insert("USER".into(), account.name.clone());
        environment.insert("LOGNAME".into(), account.name);
        environment.insert("SHELL".into(), account.shell);
        let identity = (self.options.mode == ManagerMode::System).then_some(&account.identity);
        let process = match SpawnedProcess::spawn(definition, identity, &environment) {
            Ok(process) => process,
            Err(error) => {
                eprintln!("loom: {service}: {error}");
                return RuntimeEvent::SpawnFailed {
                    service: service.clone(),
                    generation,
                    at_ms: now,
                };
            }
        };
        if let Err(error) = self.register_process(service.clone(), generation, process) {
            eprintln!("loom: {service}: {error}");
            return RuntimeEvent::SpawnFailed {
                service: service.clone(),
                generation,
                at_ms: now,
            };
        }
        RuntimeEvent::ExecSucceeded {
            service: service.clone(),
            generation,
            at_ms: now,
        }
    }

    fn register_process(
        &mut self,
        service: ServiceId,
        generation: u64,
        mut process: SpawnedProcess,
    ) -> io::Result<()> {
        let pid_token = self.allocate_token();
        if let Err(error) = self.reactor.add(process.pidfd().as_fd(), pid_token, false) {
            let _ = process.terminate(true);
            let _ = process.wait();
            return Err(error);
        }
        self.sources.insert(
            pid_token,
            Source::Process {
                service: service.clone(),
                generation,
            },
        );
        let notify_token = if let Some(notification) = process.notification_fd() {
            let token = self.allocate_token();
            if let Err(error) = self.reactor.add(notification.as_fd(), token, false) {
                let _ = self.reactor.remove(process.pidfd().as_raw_fd());
                self.sources.remove(&pid_token);
                let _ = process.terminate(true);
                let _ = process.wait();
                return Err(error);
            }
            self.sources.insert(
                token,
                Source::Notify {
                    service: service.clone(),
                    generation,
                    token,
                },
            );
            Some(token)
        } else {
            None
        };
        self.processes.insert(
            service,
            ProcessSlot {
                process,
                generation,
                pid_token,
                notify_token,
            },
        );
        Ok(())
    }

    fn handle_notification(
        &mut self,
        service: &ServiceId,
        generation: u64,
        token: u64,
    ) -> Result<(), ManagerError> {
        let notification = self
            .processes
            .get(service)
            .filter(|slot| slot.generation == generation)
            .map(|slot| slot.process.read_notification())
            .transpose()?;
        let Some(notification) = notification else {
            return Ok(());
        };
        let (event, close) = match notification {
            NotificationRead::Pending => return Ok(()),
            NotificationRead::Message(message) if message == b"READY" => (
                RuntimeEvent::Ready {
                    service: service.clone(),
                    generation,
                    at_ms: monotonic_ms()?,
                },
                true,
            ),
            NotificationRead::Message(message) if message.starts_with(b"STATUS ") => {
                return Ok(());
            }
            NotificationRead::Message(_) | NotificationRead::Closed => (
                RuntimeEvent::ReadinessFailed {
                    service: service.clone(),
                    generation,
                    at_ms: monotonic_ms()?,
                },
                true,
            ),
        };
        if close {
            self.close_notification(service, token)?;
        }
        let effects = self.engine.handle(event);
        self.apply_effects(effects)
    }

    fn close_notification(&mut self, service: &ServiceId, token: u64) -> io::Result<()> {
        self.sources.remove(&token);
        if let Some(slot) = self.processes.get_mut(service)
            && let Some(descriptor) = slot.process.take_notification()
        {
            self.reactor.remove(descriptor.as_raw_fd())?;
            slot.notify_token = None;
        }
        Ok(())
    }

    fn handle_process_exit(
        &mut self,
        service: &ServiceId,
        generation: u64,
    ) -> Result<(), ManagerError> {
        let status = self
            .processes
            .get_mut(service)
            .filter(|slot| slot.generation == generation)
            .map(|slot| slot.process.try_wait())
            .transpose()
            .map_err(process_io)?
            .flatten();
        if let Some(status) = status {
            self.finish_process(service, generation, status)?;
        }
        Ok(())
    }

    fn collect_process_exits(&mut self) -> Result<(), ManagerError> {
        let services = self.processes.keys().cloned().collect::<Vec<_>>();
        for service in services {
            let generation = self.processes[&service].generation;
            self.handle_process_exit(&service, generation)?;
        }
        while reap_exited_child()?.is_some() {}
        Ok(())
    }

    fn finish_process(
        &mut self,
        service: &ServiceId,
        generation: u64,
        status: std::process::ExitStatus,
    ) -> Result<(), ManagerError> {
        let Some(mut slot) = self.processes.remove(service) else {
            return Ok(());
        };
        self.reactor.remove(slot.process.pidfd().as_raw_fd())?;
        self.sources.remove(&slot.pid_token);
        if let Some(token) = slot.notify_token {
            self.sources.remove(&token);
            if let Some(descriptor) = slot.process.take_notification() {
                self.reactor.remove(descriptor.as_raw_fd())?;
            }
        }
        let outcome = if status.success() {
            ExitOutcome::Success
        } else if let Some(signal) = status.signal() {
            ExitOutcome::Signal(signal)
        } else {
            ExitOutcome::ExitCode(status.code().unwrap_or(1))
        };
        let effects = self.engine.handle(RuntimeEvent::Exited {
            service: service.clone(),
            generation,
            outcome,
            at_ms: monotonic_ms()?,
        });
        self.apply_effects(effects)
    }

    fn push_deadline(&mut self, when_ms: u64, action: DeadlineAction) -> io::Result<()> {
        let sequence = self.next_deadline_sequence;
        self.next_deadline_sequence = self.next_deadline_sequence.wrapping_add(1);
        self.deadlines.push(Deadline {
            when_ms,
            sequence,
            action,
        });
        self.arm_next_deadline()
    }

    fn arm_next_deadline(&self) -> io::Result<()> {
        let Some(deadline) = self.deadlines.peek() else {
            return self.timer.disarm();
        };
        let after = deadline.when_ms.saturating_sub(monotonic_ms()?);
        self.timer.arm(Duration::from_millis(after))
    }

    fn handle_deadlines(&mut self) -> Result<(), ManagerError> {
        let _ = self.timer.consume()?;
        let now = monotonic_ms()?;
        while self
            .deadlines
            .peek()
            .is_some_and(|deadline| deadline.when_ms <= now)
        {
            let deadline = self.deadlines.pop().expect("peeked deadline exists");
            match deadline.action {
                DeadlineAction::Runtime {
                    service,
                    generation,
                    kind,
                } => {
                    let effects = self.engine.handle(RuntimeEvent::Timer {
                        service,
                        generation,
                        kind,
                        at_ms: now,
                    });
                    self.apply_effects(effects)?;
                }
                DeadlineAction::Client { token, request_id } => {
                    let timed_out = self
                        .clients
                        .get(&token)
                        .and_then(|client| client.pending.as_ref())
                        .is_some_and(|pending| pending.request_id == request_id);
                    if timed_out {
                        let (operation, payload) = {
                            let client = self.clients.get_mut(&token).expect("known client");
                            let pending = client.pending.take().expect("checked pending request");
                            (pending.operation, b"operation timed out".to_vec())
                        };
                        self.respond(token, request_id, operation, StatusCode::Timeout, payload);
                    }
                }
            }
        }
        self.arm_next_deadline()?;
        Ok(())
    }

    fn allocate_token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1).max(FIRST_DYNAMIC_TOKEN);
        token
    }

    fn remove_client(&mut self, token: u64) {
        if let Some(client) = self.clients.remove(&token) {
            let _ = self.reactor.remove(client.connection.as_fd().as_raw_fd());
        }
        self.sources.remove(&token);
    }
}

#[derive(Clone)]
enum Source {
    Listener,
    Signal,
    Timer,
    Client(u64),
    Process {
        service: ServiceId,
        generation: u64,
    },
    Notify {
        service: ServiceId,
        generation: u64,
        token: u64,
    },
}

struct Client {
    connection: SeqPacketConnection,
    credentials_uid: u32,
    pending: Option<PendingRequest>,
}

struct ProcessSlot {
    process: SpawnedProcess,
    generation: u64,
    pid_token: u64,
    notify_token: Option<u64>,
}

struct PendingRequest {
    request_id: u64,
    operation: Operation,
    deadline_ms: u64,
    kind: PendingKind,
}

impl PendingRequest {
    fn start(request_id: u64, operation: Operation, services: BTreeSet<ServiceId>) -> Self {
        Self {
            request_id,
            operation,
            deadline_ms: 0,
            kind: PendingKind::Start(services),
        }
    }

    fn stop(request_id: u64, operation: Operation, services: BTreeSet<ServiceId>) -> Self {
        Self {
            request_id,
            operation,
            deadline_ms: 0,
            kind: PendingKind::Stop(services),
        }
    }

    fn restart(
        request_id: u64,
        operation: Operation,
        service: ServiceId,
        previous_generation: u64,
    ) -> Self {
        Self {
            request_id,
            operation,
            deadline_ms: 0,
            kind: PendingKind::Restart {
                service,
                previous_generation,
            },
        }
    }
}

enum PendingKind {
    Start(BTreeSet<ServiceId>),
    Stop(BTreeSet<ServiceId>),
    Restart {
        service: ServiceId,
        previous_generation: u64,
    },
}

#[derive(Eq, PartialEq)]
struct Deadline {
    when_ms: u64,
    sequence: u64,
    action: DeadlineAction,
}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .when_ms
            .cmp(&self.when_ms)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Eq, PartialEq)]
enum DeadlineAction {
    Runtime {
        service: ServiceId,
        generation: u64,
        kind: TimerKind,
    },
    Client {
        token: u64,
        request_id: u64,
    },
}

fn payload_target(packet: &Packet) -> Result<ServiceId, ManagerError> {
    let target = std::str::from_utf8(&packet.payload).map_err(|_| {
        ManagerError::InvalidTarget(String::from_utf8_lossy(&packet.payload).into())
    })?;
    ServiceId::new(target.trim()).map_err(|_| ManagerError::InvalidTarget(target.to_owned()))
}

const fn is_mutating(operation: Operation) -> bool {
    !matches!(
        operation,
        Operation::Status
            | Operation::List
            | Operation::IsActive
            | Operation::IsEnabled
            | Operation::Dependencies
            | Operation::Timings
            | Operation::CriticalPath
    )
}

const fn observed_name(state: ObservedState) -> &'static str {
    match state {
        ObservedState::Inactive => "inactive",
        ObservedState::Starting => "starting",
        ObservedState::Active => "active",
        ObservedState::Stopping => "stopping",
        ObservedState::Failed => "failed",
    }
}

const fn desired_name(state: DesiredState) -> &'static str {
    match state {
        DesiredState::Inactive => "inactive",
        DesiredState::Active => "active",
    }
}

fn control_directory(options: &ManagerOptions) -> PathBuf {
    options.runtime_dir.join("loom")
}

fn create_control_directory(path: &Path, mode: ManagerMode, uid: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(match mode {
            ManagerMode::System => 0o755,
            ManagerMode::User => 0o700,
        }),
    )?;
    let metadata = fs::symlink_metadata(path)?;
    let expected = if mode == ManagerMode::System { 0 } else { uid };
    if !metadata.file_type().is_dir() || metadata.uid() != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Loom control directory",
        ));
    }
    Ok(())
}

fn protocol_io(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn process_io(error: ProcessError) -> ManagerError {
    ManagerError::Io(io::Error::other(error))
}
