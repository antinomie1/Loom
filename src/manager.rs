// SPDX-License-Identifier: BSD-2-Clause

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque},
    fmt::Write as _,
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
    config::ConfigSnapshot,
    config_edit::{EditError, atomic_write, edit_enabled},
    identity::{AccountDatabase, IdentityError},
    linux::{
        CgroupDomain, NotificationRead, ProcessError, SpawnedProcess, current_identity,
        prepare_cgroup_root,
        reactor::{
            Reactor, SeqPacketConnection, SeqPacketListener, SignalFd, TimerFd, monotonic_ms,
        },
        reap_exited_child,
    },
    loader::{ConfigLoader, LoadError},
    model::{ManagerScope, Readiness, ResolvedIdentity, ServiceId},
    protocol::{MessageKind, Operation, Packet, ProtocolError, StatusCode},
    runtime::{
        DesiredState, ExitOutcome, ObservedState, RuntimeEffect, RuntimeEngine, RuntimeError,
        RuntimeEvent, ServiceStatus, TimerKind,
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

struct ShutdownState {
    action: ShutdownAction,
    phase: ShutdownPhase,
}

enum ShutdownPhase {
    Running(BTreeSet<ServiceId>),
    Stopping,
}

#[derive(Clone, Copy)]
enum ServiceAction {
    Stop,
    Reload,
}

impl ServiceAction {
    const fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Reload => "reload",
        }
    }
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
    #[error(transparent)]
    Edit(#[from] EditError),
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
    cgroup_root: Option<PathBuf>,
    deadlines: BinaryHeap<Deadline>,
    next_token: u64,
    next_deadline_sequence: u64,
    shutdown: Option<ShutdownState>,
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
        let cgroup_root = if options.root == Path::new("/") {
            match options.mode {
                ManagerMode::System => Some(prepare_cgroup_root(true, owner_identity.uid)?),
                ManagerMode::User => match prepare_cgroup_root(false, owner_identity.uid) {
                    Ok(root) => Some(root),
                    Err(error) => {
                        eprintln!("loom: user cgroups unavailable: {error}");
                        None
                    }
                },
            }
        } else {
            None
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
            cgroup_root,
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
            if let Some(action) = self.progress_shutdown()? {
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
            Err(
                error @ (ManagerError::Runtime(_)
                | ManagerError::InvalidTarget(_)
                | ManagerError::Config(_)
                | ManagerError::Edit(_)),
            ) => {
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
            Operation::Start | Operation::Stop | Operation::Restart | Operation::ReloadService
        ) {
            self.dispatch_lifecycle(token, request)?;
        } else if matches!(
            request.operation,
            Operation::Status
                | Operation::List
                | Operation::IsActive
                | Operation::IsEnabled
                | Operation::Dependencies
                | Operation::Timings
                | Operation::CriticalPath
        ) {
            self.dispatch_query(token, request)?;
        } else if matches!(
            request.operation,
            Operation::Enable
                | Operation::Disable
                | Operation::Reload
                | Operation::Apply
                | Operation::ResetFailed
        ) {
            self.dispatch_configuration(token, request)?;
        } else if matches!(request.operation, Operation::Reboot | Operation::Poweroff)
            && self.options.mode == ManagerMode::System
        {
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
        } else {
            self.respond(
                token,
                request.request_id,
                request.operation,
                StatusCode::InvalidRequest,
                b"operation is not implemented yet".to_vec(),
            );
        }
        self.complete_pending_clients();
        Ok(())
    }

    fn dispatch_query(&mut self, token: u64, request: &Packet) -> Result<(), ManagerError> {
        let (status, payload) = match request.operation {
            Operation::Status | Operation::List => {
                (StatusCode::Ok, self.status_payload(&request.payload))
            }
            Operation::IsActive => {
                let service = payload_target(request)?;
                let active = self
                    .engine
                    .status(&service)
                    .is_some_and(|status| status.observed == ObservedState::Active);
                (
                    if active {
                        StatusCode::Ok
                    } else {
                        StatusCode::ServiceFailure
                    },
                    if active { "active" } else { "inactive" }
                        .as_bytes()
                        .to_vec(),
                )
            }
            Operation::IsEnabled => {
                let service = payload_target(request)?;
                let enabled = self.engine.snapshot().is_enabled(&service);
                (
                    if enabled {
                        StatusCode::Ok
                    } else {
                        StatusCode::ServiceFailure
                    },
                    if enabled { "enabled" } else { "disabled" }
                        .as_bytes()
                        .to_vec(),
                )
            }
            Operation::Dependencies => {
                let service = payload_target(request)?;
                let dependencies = self
                    .engine
                    .snapshot()
                    .dependencies(&service)
                    .ok_or_else(|| RuntimeError::UnknownTarget(service.clone()))?;
                (
                    StatusCode::Ok,
                    format!(
                        "requires={}\nwants={}\nafter={}\nconflicts={}\n",
                        join_ids(&dependencies.requires),
                        join_ids(&dependencies.wants),
                        join_ids(&dependencies.after),
                        join_ids(&dependencies.conflicts),
                    )
                    .into_bytes(),
                )
            }
            Operation::Timings => (StatusCode::Ok, self.timings_payload()),
            Operation::CriticalPath => (StatusCode::Ok, self.critical_path_payload()),
            _ => unreachable!("query dispatcher receives query operations"),
        };
        self.respond(
            token,
            request.request_id,
            request.operation,
            status,
            payload,
        );
        Ok(())
    }

    fn dispatch_configuration(&mut self, token: u64, request: &Packet) -> Result<(), ManagerError> {
        let payload = match request.operation {
            Operation::Enable | Operation::Disable => {
                let (service, now) = enabled_payload(request)?;
                let enabled = request.operation == Operation::Enable;
                self.change_enabled(&service, enabled, now)?;
                if enabled { "enabled" } else { "disabled" }
                    .as_bytes()
                    .to_vec()
            }
            Operation::Reload => {
                self.reload_configuration(false)?;
                b"reloaded".to_vec()
            }
            Operation::Apply => {
                self.reload_configuration(true)?;
                let services = self
                    .engine
                    .snapshot()
                    .activation_services(self.engine.snapshot().default_group())
                    .unwrap_or_default();
                self.set_pending(
                    token,
                    PendingRequest::apply(request.request_id, request.operation, services),
                )?;
                return Ok(());
            }
            Operation::ResetFailed => {
                self.engine.reset_failed(&payload_target(request)?)?;
                b"reset".to_vec()
            }
            _ => unreachable!("configuration dispatcher receives configuration operations"),
        };
        self.respond(
            token,
            request.request_id,
            request.operation,
            StatusCode::Ok,
            payload,
        );
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
            Operation::ReloadService => {
                let service = payload_target(request)?;
                let success = match self.run_service_action(&service, ServiceAction::Reload) {
                    Ok(success) => success,
                    Err(error) => {
                        eprintln!("loom: {service}: reload action failed: {error}");
                        false
                    }
                };
                self.respond(
                    token,
                    request.request_id,
                    request.operation,
                    if success {
                        StatusCode::Ok
                    } else {
                        StatusCode::ServiceFailure
                    },
                    if success { "reloaded" } else { "reload failed" }
                        .as_bytes()
                        .to_vec(),
                );
            }
            _ => unreachable!("lifecycle dispatcher receives lifecycle operations"),
        }
        Ok(())
    }

    fn change_enabled(
        &mut self,
        service: &ServiceId,
        enabled: bool,
        now: bool,
    ) -> Result<(), ManagerError> {
        if enabled && !self.engine.snapshot().services().contains_key(service) {
            return Err(RuntimeError::UnknownTarget(service.clone()).into());
        }
        let path = self.manager_config_path()?;
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && self.options.mode == ManagerMode::User =>
            {
                "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = []\n"
                    .to_owned()
            }
            Err(error) => return Err(ManagerError::Io(error)),
        };
        let edited = edit_enabled(&source, service, enabled)?;
        let snapshot = self.load_with_manager(&edited)?;
        let previous = Arc::clone(self.engine.snapshot());
        self.engine.replace_snapshot(Arc::new(snapshot))?;
        if let Err(error) = atomic_write(&path, &edited, 0o644) {
            let _ = self.engine.replace_snapshot(previous);
            return Err(error.into());
        }
        if now {
            let effects = if enabled {
                self.engine.start(service, monotonic_ms()?)?
            } else {
                self.engine.stop(service)?
            };
            self.apply_effects(effects)?;
        }
        Ok(())
    }

    fn reload_configuration(&mut self, reconcile: bool) -> Result<(), ManagerError> {
        let snapshot = match self.options.mode {
            ManagerMode::System => ConfigLoader::load_system(&self.options.root)?,
            ManagerMode::User => ConfigLoader::load_user(
                &self.options.root,
                self.options
                    .config_home
                    .as_deref()
                    .ok_or(ManagerError::InvalidOptions(
                        "user manager requires config_home",
                    ))?,
                &self.options.runtime_dir,
                self.owner_identity.uid,
            )?,
        };
        if reconcile {
            let effects = self
                .engine
                .apply_snapshot(Arc::new(snapshot), monotonic_ms()?)?;
            self.apply_effects(effects)?;
        } else {
            self.engine.replace_snapshot(Arc::new(snapshot))?;
        }
        Ok(())
    }

    fn load_with_manager(&self, source: &str) -> Result<ConfigSnapshot, ManagerError> {
        match self.options.mode {
            ManagerMode::System => Ok(ConfigLoader::load_system_with_manager(
                &self.options.root,
                source,
            )?),
            ManagerMode::User => Ok(ConfigLoader::load_user_with_manager(
                &self.options.root,
                self.options
                    .config_home
                    .as_deref()
                    .ok_or(ManagerError::InvalidOptions(
                        "user manager requires config_home",
                    ))?,
                &self.options.runtime_dir,
                self.owner_identity.uid,
                source,
            )?),
        }
    }

    fn manager_config_path(&self) -> Result<PathBuf, ManagerError> {
        match self.options.mode {
            ManagerMode::System => Ok(self.options.root.join("etc/loom/loom.toml")),
            ManagerMode::User => self
                .options
                .config_home
                .as_ref()
                .map(|home| home.join("loom/loom.toml"))
                .ok_or(ManagerError::InvalidOptions(
                    "user manager requires config_home",
                )),
        }
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
            PendingKind::Start(services) | PendingKind::Apply(services) => {
                let statuses = services
                    .iter()
                    .filter_map(|service| self.engine.status(service))
                    .collect::<Vec<_>>();
                if matches!(pending.kind, PendingKind::Apply(_)) && self.engine.apply_pending() {
                    None
                } else if statuses.iter().any(|status| {
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

    fn timings_payload(&self) -> Vec<u8> {
        let statuses = self.engine.statuses().collect::<Vec<_>>();
        let base = statuses
            .iter()
            .filter_map(|(_, status)| status.queued_at_ms)
            .min()
            .unwrap_or(0);
        let mut output =
            String::from("service\tqueued_ms\tstarted_ms\tready_ms\texited_ms\tstartup_ms\n");
        for (service, status) in statuses {
            let startup = status
                .ready_at_ms
                .zip(status.started_at_ms)
                .map(|(ready, started)| ready.saturating_sub(started));
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{}",
                service,
                relative_time(status.queued_at_ms, base),
                relative_time(status.started_at_ms, base),
                relative_time(status.ready_at_ms, base),
                relative_time(status.exited_at_ms, base),
                optional_number(startup),
            )
            .expect("writing to String cannot fail");
        }
        output.into_bytes()
    }

    fn critical_path_payload(&self) -> Vec<u8> {
        let statuses = self
            .engine
            .statuses()
            .map(|(service, status)| (service.clone(), status))
            .collect::<BTreeMap<_, _>>();
        let mut memo = BTreeMap::new();
        let best = statuses
            .iter()
            .filter(|(_, status)| status.ready_at_ms.is_some())
            .map(|(service, _)| {
                critical_path_for(service, self.engine.snapshot(), &statuses, &mut memo)
            })
            .max_by_key(|(duration, _)| *duration)
            .unwrap_or_default();
        let services = best
            .1
            .iter()
            .map(ServiceId::as_str)
            .collect::<Vec<_>>()
            .join(" -> ");
        format!("duration_ms={}\nservices={}\n", best.0, services).into_bytes()
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
                libc::SIGHUP => {
                    if let Err(error) = self.reload_configuration(false) {
                        eprintln!("loom: reload failed: {error}");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn begin_shutdown(&mut self, action: ShutdownAction) -> Result<(), ManagerError> {
        if self.shutdown.is_some() {
            return Ok(());
        }
        let shutdown_group = self.engine.snapshot().shutdown_group().cloned();
        if let Some(group) = shutdown_group {
            let pending = self
                .engine
                .snapshot()
                .activation_services(&group)
                .ok_or_else(|| RuntimeError::UnknownTarget(group.clone()))?
                .into_iter()
                .filter(|service| {
                    self.engine
                        .status(service)
                        .is_none_or(|status| status.observed != ObservedState::Active)
                })
                .collect();
            self.shutdown = Some(ShutdownState {
                action,
                phase: ShutdownPhase::Running(pending),
            });
            let effects = self.engine.start(&group, monotonic_ms()?)?;
            self.apply_effects(effects)
        } else {
            self.shutdown = Some(ShutdownState {
                action,
                phase: ShutdownPhase::Stopping,
            });
            let effects = self.engine.stop_all();
            self.apply_effects(effects)
        }
    }

    fn progress_shutdown(&mut self) -> Result<Option<ShutdownAction>, ManagerError> {
        let Some(shutdown) = &self.shutdown else {
            return Ok(None);
        };
        match &shutdown.phase {
            ShutdownPhase::Running(pending)
                if pending.iter().all(|service| {
                    self.engine.status(service).is_some_and(|status| {
                        status.observed == ObservedState::Failed
                            || (status.observed == ObservedState::Active
                                && !self.processes.contains_key(service))
                    })
                }) =>
            {
                if let Some(shutdown) = &mut self.shutdown {
                    shutdown.phase = ShutdownPhase::Stopping;
                }
                let action = self.shutdown.as_ref().expect("shutdown exists").action;
                let effects = self.engine.stop_all();
                self.apply_effects(effects)?;
                if self.processes.is_empty()
                    && self.engine.statuses().all(|(_, status)| {
                        matches!(
                            status.observed,
                            ObservedState::Inactive | ObservedState::Failed
                        )
                    })
                {
                    Ok(Some(action))
                } else {
                    Ok(None)
                }
            }
            ShutdownPhase::Stopping
                if self.processes.is_empty()
                    && self.engine.statuses().all(|(_, status)| {
                        matches!(
                            status.observed,
                            ObservedState::Inactive | ObservedState::Failed
                        )
                    }) =>
            {
                Ok(Some(shutdown.action))
            }
            _ => Ok(None),
        }
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
                    if !force
                        && self.engine.snapshot().services()[&service]
                            .actions
                            .stop
                            .is_some()
                    {
                        match self.run_service_action(&service, ServiceAction::Stop) {
                            Ok(true) => {}
                            Ok(false) => eprintln!("loom: {service}: stop action failed"),
                            Err(error) => {
                                eprintln!("loom: {service}: stop action failed: {error}");
                            }
                        }
                    }
                    if let Some(slot) = self.processes.get(&service)
                        && slot.generation == generation
                    {
                        if let Some(cgroup) = &slot.cgroup
                            && let Err(error) = cgroup.terminate(force)
                        {
                            eprintln!("loom: {service}: cannot signal cgroup: {error}");
                        }
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

    fn run_service_action(
        &mut self,
        service: &ServiceId,
        action: ServiceAction,
    ) -> Result<bool, ManagerError> {
        let definition = self
            .engine
            .snapshot()
            .services()
            .get(service)
            .ok_or_else(|| RuntimeError::UnknownTarget(service.clone()))?;
        let command = match action {
            ServiceAction::Stop => definition.actions.stop.clone(),
            ServiceAction::Reload => definition.actions.reload.clone(),
        }
        .ok_or_else(|| {
            ManagerError::InvalidTarget(format!("{service} has no {} action", action.name()))
        })?;
        let main_pid = self
            .processes
            .get(service)
            .map(|slot| slot.process.pid())
            .ok_or_else(|| ManagerError::InvalidTarget(format!("{service} is not running")))?;
        let account = self.accounts.resolve(
            &definition.process,
            match self.options.mode {
                ManagerMode::System => ManagerScope::System,
                ManagerMode::User => ManagerScope::User,
            },
            &self.owner_identity,
        )?;
        let mut environment = self.base_environment.clone();
        environment.insert("HOME".into(), account.home);
        environment.insert("USER".into(), account.name.clone());
        environment.insert("LOGNAME".into(), account.name);
        environment.insert("SHELL".into(), account.shell);
        environment.insert("LOOM_MAINPID".into(), main_pid.to_string());
        let identity = (self.options.mode == ManagerMode::System).then_some(&account.identity);
        let mut action_definition = definition.clone();
        action_definition.process.command = command;
        action_definition.process.readiness = Readiness::Exec;
        let timeout_ms = match action {
            ServiceAction::Stop => definition.supervision.stop_timeout_ms,
            ServiceAction::Reload => definition.supervision.start_timeout_ms,
        };
        let mut process = SpawnedProcess::spawn(&action_definition, identity, &environment, None)
            .map_err(process_io)?;
        let deadline = monotonic_ms()?.saturating_add(timeout_ms);
        loop {
            if let Some(status) = process.try_wait().map_err(process_io)? {
                return Ok(status.success());
            }
            if monotonic_ms()? >= deadline {
                process.terminate(true).map_err(process_io)?;
                let _ = process.wait().map_err(process_io)?;
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
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
        let cgroup = match self
            .cgroup_root
            .as_deref()
            .map(|root| CgroupDomain::create(root, service.as_str(), generation))
            .transpose()
        {
            Ok(cgroup) => cgroup,
            Err(error) => {
                eprintln!("loom: {service}: cannot create cgroup: {error}");
                return RuntimeEvent::SpawnFailed {
                    service: service.clone(),
                    generation,
                    at_ms: now,
                };
            }
        };
        let process =
            match SpawnedProcess::spawn(definition, identity, &environment, cgroup.as_ref()) {
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
        if let Err(error) = self.register_process(service.clone(), generation, process, cgroup) {
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
        cgroup: Option<CgroupDomain>,
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
                cgroup,
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
        if let Some(cgroup) = &slot.cgroup
            && let Err(error) = cgroup.terminate(true)
        {
            eprintln!("loom: {service}: cannot clean cgroup: {error}");
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
    cgroup: Option<CgroupDomain>,
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

    fn apply(request_id: u64, operation: Operation, services: BTreeSet<ServiceId>) -> Self {
        Self {
            request_id,
            operation,
            deadline_ms: 0,
            kind: PendingKind::Apply(services),
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
    Apply(BTreeSet<ServiceId>),
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

fn enabled_payload(packet: &Packet) -> Result<(ServiceId, bool), ManagerError> {
    let payload = std::str::from_utf8(&packet.payload).map_err(|_| {
        ManagerError::InvalidTarget(String::from_utf8_lossy(&packet.payload).into())
    })?;
    let (target, now) = payload
        .strip_prefix("now\n")
        .map_or((payload, false), |target| (target, true));
    let service = ServiceId::new(target.trim())
        .map_err(|_| ManagerError::InvalidTarget(target.to_owned()))?;
    Ok((service, now))
}

fn payload_target(packet: &Packet) -> Result<ServiceId, ManagerError> {
    let target = std::str::from_utf8(&packet.payload).map_err(|_| {
        ManagerError::InvalidTarget(String::from_utf8_lossy(&packet.payload).into())
    })?;
    ServiceId::new(target.trim()).map_err(|_| ManagerError::InvalidTarget(target.to_owned()))
}

fn relative_time(value: Option<u64>, base: u64) -> String {
    value.map_or_else(
        || "-".into(),
        |value| value.saturating_sub(base).to_string(),
    )
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

fn critical_path_for(
    service: &ServiceId,
    snapshot: &ConfigSnapshot,
    statuses: &BTreeMap<ServiceId, ServiceStatus>,
    memo: &mut BTreeMap<ServiceId, (u64, Vec<ServiceId>)>,
) -> (u64, Vec<ServiceId>) {
    if let Some(path) = memo.get(service) {
        return path.clone();
    }
    let own = statuses
        .get(service)
        .and_then(|status| status.ready_at_ms.zip(status.started_at_ms))
        .map_or(0, |(ready, started)| ready.saturating_sub(started));
    let mut path = snapshot
        .dependencies(service)
        .into_iter()
        .flat_map(|dependencies| dependencies.requires.iter().chain(&dependencies.after))
        .filter(|dependency| statuses.contains_key(*dependency))
        .map(|dependency| critical_path_for(dependency, snapshot, statuses, memo))
        .max_by_key(|(duration, _)| *duration)
        .unwrap_or_default();
    path.0 = path.0.saturating_add(own);
    path.1.push(service.clone());
    memo.insert(service.clone(), path.clone());
    path
}

fn join_ids(ids: &[ServiceId]) -> String {
    ids.iter()
        .map(ServiceId::as_str)
        .collect::<Vec<_>>()
        .join(",")
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
