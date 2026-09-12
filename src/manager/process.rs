// SPDX-License-Identifier: BSD-2-Clause

use super::{
    CgroupDomain, DeadlineAction, Manager, ManagerError, ManagerMode, ManagerScope, PendingKind,
    ProcessSlot, Reactor, Readiness, RuntimeError, ServiceAction, ServiceId, Source,
    SpawnedProcess, StatusCode, monotonic_ms, process_io,
};
use std::{
    collections::HashMap,
    io,
    os::fd::{AsFd, AsRawFd},
};

impl ProcessSlot {
    pub(super) fn signal(&self, force: bool) -> Result<(), ManagerError> {
        if let Some(cgroup) = &self.cgroup {
            cgroup.terminate(force)?;
        }
        self.process.terminate(force).map_err(process_io)
    }

    pub(super) fn poll_exit(&mut self, reactor: &Reactor) -> Result<bool, ManagerError> {
        if self.exit_status.is_none() {
            let Some(status) = self.process.try_wait().map_err(process_io)? else {
                // Read also acknowledges any cgroup.events notification.
                if let Some(cgroup) = &self.cgroup {
                    let _ = cgroup.is_empty()?;
                }
                return Ok(false);
            };
            self.exit_status = Some(status);
            reactor.remove(self.process.pidfd().as_raw_fd())?;
            if let Some(descriptor) = self.process.take_notification() {
                reactor.remove(descriptor.as_raw_fd())?;
            }
            // A leader's exit never leaves descendants running, including fallback mode.
            self.signal(true)?;
        }
        match &self.cgroup {
            Some(cgroup) => Ok(cgroup.is_empty()?),
            None => Ok(self.process.group_is_empty()?),
        }
    }

    pub(super) fn unregister(&self, reactor: &Reactor, sources: &mut HashMap<u64, Source>) {
        let _ = reactor.remove(self.process.pidfd().as_raw_fd());
        sources.remove(&self.pid_token);
        if let Some(token) = self.notify_token {
            sources.remove(&token);
            if let Some(fd) = self.process.notification_fd() {
                let _ = reactor.remove(fd.as_raw_fd());
            }
        }
        if let Some(token) = self.domain_token {
            sources.remove(&token);
            if let Some(cgroup) = &self.cgroup {
                let _ = reactor.remove(cgroup.events().as_raw_fd());
            }
        }
    }
}

impl Manager {
    pub(super) fn start_service_action(
        &mut self,
        service: &ServiceId,
        action: ServiceAction,
    ) -> Result<u64, ManagerError> {
        let main = self
            .processes
            .get(service)
            .ok_or_else(|| ManagerError::InvalidTarget(format!("{service} is not running")))?;
        let generation = main.generation;
        if let Some((id, _)) = self.actions.iter().find(|(_, slot)| {
            slot.service == *service && slot.generation == generation && slot.action == action
        }) {
            return Ok(*id);
        }
        if self.actions.values().any(|slot| slot.service == *service) {
            return Err(RuntimeError::ApplyInProgress.into());
        }
        let main_pid = main.process.pid();
        let definition = &self.engine.snapshot().services()[service];
        let command = match action {
            ServiceAction::Stop => definition.actions.stop.clone(),
            ServiceAction::Reload => definition.actions.reload.clone(),
        }
        .ok_or_else(|| {
            ManagerError::InvalidTarget(format!("{service} has no {} action", action.name()))
        })?;
        let account =
            self.accounts
                .resolve(&definition.process, self.scope(), &self.owner_identity)?;
        let mut environment = self.base_environment.clone();
        environment.insert("HOME".into(), account.home);
        environment.insert("USER".into(), account.name.clone());
        environment.insert("LOGNAME".into(), account.name);
        environment.insert("SHELL".into(), account.shell);
        environment.insert("LOOM_MAINPID".into(), main_pid.to_string());
        let mut definition = definition.clone();
        definition.process.command = command;
        definition.process.readiness = Readiness::Exec;
        let timeout_ms = match action {
            ServiceAction::Stop => definition.supervision.stop_timeout_ms,
            ServiceAction::Reload => definition.supervision.start_timeout_ms,
        };
        let id = self.allocate_token();
        let cgroup = self
            .cgroup_root
            .as_deref()
            .map(|root| CgroupDomain::create(root, &format!("action-{id}"), 0))
            .transpose()?;
        let identity = (self.options.mode == ManagerMode::System).then_some(&account.identity);
        let mut process =
            SpawnedProcess::spawn(&definition, identity, &environment, cgroup.as_ref())
                .map_err(process_io)?;
        if let Err(error) = self.reactor.add(process.pidfd().as_fd(), id, false) {
            let _ = process.terminate(true);
            let _ = process.wait();
            return Err(error.into());
        }
        let domain_token = match self.register_domain(cgroup.as_ref(), Source::Action(id)) {
            Ok(token) => token,
            Err(error) => {
                let _ = self.reactor.remove(process.pidfd().as_fd().as_raw_fd());
                if let Some(domain) = &cgroup {
                    let _ = domain.terminate(true);
                }
                let _ = process.terminate(true);
                let _ = process.wait();
                return Err(error.into());
            }
        };
        self.sources.insert(id, Source::Action(id));
        self.actions.insert(
            id,
            ActionSlot {
                service: service.clone(),
                generation,
                action,
                timed_out: false,
                process: ProcessSlot {
                    process,
                    generation,
                    pid_token: id,
                    notify_token: None,
                    domain_token,
                    cgroup,
                    exit_status: None,
                },
            },
        );
        if timeout_ms > 0 {
            self.push_deadline(
                monotonic_ms()?.saturating_add(timeout_ms),
                DeadlineAction::Action(id),
            )?;
        }
        Ok(id)
    }

    pub(super) fn register_domain(
        &mut self,
        cgroup: Option<&CgroupDomain>,
        source: Source,
    ) -> io::Result<Option<u64>> {
        let Some(cgroup) = cgroup else {
            return Ok(None);
        };
        let token = self.allocate_token();
        self.reactor.add_priority(cgroup.events().as_fd(), token)?;
        self.sources.insert(token, source);
        Ok(Some(token))
    }

    pub(super) fn handle_action_exit(&mut self, id: u64) -> Result<(), ManagerError> {
        let Some(action) = self.actions.get_mut(&id) else {
            return Ok(());
        };
        if !action.process.poll_exit(&self.reactor)? {
            return Ok(());
        }
        let action = self.actions.remove(&id).expect("polled action");
        action.process.unregister(&self.reactor, &mut self.sources);
        let result = if action.timed_out {
            StatusCode::Timeout
        } else if action.process.exit_status.is_some_and(|s| s.success()) {
            StatusCode::Ok
        } else {
            StatusCode::ServiceFailure
        };
        for client in self.clients.values_mut() {
            if let Some(pending) = &mut client.pending
                && let PendingKind::Action {
                    id: pending_id,
                    result: pending_result,
                } = &mut pending.kind
                && *pending_id == id
            {
                *pending_result = Some(result);
            }
        }
        if action.action == ServiceAction::Stop {
            if result != StatusCode::Ok {
                eprintln!("loom: {}: stop action failed: {result:?}", action.service);
            }
            if let Some(main) = self.processes.get(&action.service)
                && main.generation == action.generation
            {
                main.signal(false)?;
            }
        }
        self.handle_process_exit(&action.service, action.generation)
    }

    pub(super) fn cancel_actions(
        &mut self,
        service: &ServiceId,
        generation: u64,
    ) -> Result<(), ManagerError> {
        for action in self
            .actions
            .values_mut()
            .filter(|slot| slot.service == *service && slot.generation == generation)
        {
            action.timed_out = true;
            action.process.signal(true)?;
        }
        Ok(())
    }

    pub(super) fn scope(&self) -> ManagerScope {
        match self.options.mode {
            ManagerMode::System => ManagerScope::System,
            ManagerMode::User => ManagerScope::User,
        }
    }
}

pub(super) struct ActionSlot {
    pub(super) process: ProcessSlot,
    pub(super) service: ServiceId,
    pub(super) generation: u64,
    action: ServiceAction,
    pub(super) timed_out: bool,
}
