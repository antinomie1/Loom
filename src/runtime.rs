// SPDX-License-Identifier: BSD-2-Clause

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use thiserror::Error;

use crate::{
    config::ConfigSnapshot,
    model::{ProcessType, Readiness, RestartPolicy, ServiceId},
};

const INITIAL_RESTART_BACKOFF_MS: u64 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesiredState {
    Inactive,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedState {
    Inactive,
    Starting,
    Active,
    Stopping,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TimerKind {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitOutcome {
    Success,
    ExitCode(i32),
    Signal(i32),
}

impl ExitOutcome {
    const fn failed(self) -> bool {
        !matches!(self, Self::Success)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeEvent {
    ExecSucceeded {
        service: ServiceId,
        generation: u64,
        at_ms: u64,
    },
    Ready {
        service: ServiceId,
        generation: u64,
        at_ms: u64,
    },
    SpawnFailed {
        service: ServiceId,
        generation: u64,
        at_ms: u64,
    },
    ReadinessFailed {
        service: ServiceId,
        generation: u64,
        at_ms: u64,
    },
    Exited {
        service: ServiceId,
        generation: u64,
        outcome: ExitOutcome,
        at_ms: u64,
    },
    Timer {
        service: ServiceId,
        generation: u64,
        kind: TimerKind,
        at_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeEffect {
    Spawn {
        service: ServiceId,
        generation: u64,
    },
    Terminate {
        service: ServiceId,
        generation: u64,
        force: bool,
    },
    ArmTimer {
        service: ServiceId,
        generation: u64,
        kind: TimerKind,
        after_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceStatus {
    pub desired: DesiredState,
    pub observed: ObservedState,
    pub generation: u64,
    pub blocked_by: Option<ServiceId>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RuntimeError {
    #[error("unknown service or group {0}")]
    UnknownTarget(ServiceId),
    #[error("operation requires a service, not group {0}")]
    NotService(ServiceId),
}

pub struct RuntimeEngine {
    snapshot: Arc<ConfigSnapshot>,
    services: BTreeMap<ServiceId, ServiceRuntime>,
}

impl RuntimeEngine {
    #[must_use]
    pub fn new(snapshot: Arc<ConfigSnapshot>) -> Self {
        let services = snapshot
            .services()
            .keys()
            .cloned()
            .map(|id| (id, ServiceRuntime::default()))
            .collect();
        Self { snapshot, services }
    }

    #[must_use]
    pub fn snapshot(&self) -> &Arc<ConfigSnapshot> {
        &self.snapshot
    }

    #[must_use]
    pub fn status(&self, service: &ServiceId) -> Option<ServiceStatus> {
        self.services.get(service).map(ServiceRuntime::view)
    }

    pub fn statuses(&self) -> impl Iterator<Item = (&ServiceId, ServiceStatus)> {
        self.services
            .iter()
            .map(|(service, runtime)| (service, runtime.view()))
    }

    /// Clears a terminal failure so an explicit start can try again.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::UnknownTarget`] for an unknown service.
    pub fn reset_failed(&mut self, service: &ServiceId) -> Result<(), RuntimeError> {
        let runtime = self
            .services
            .get_mut(service)
            .ok_or_else(|| RuntimeError::UnknownTarget(service.clone()))?;
        runtime.restart_blocked = false;
        runtime.blocked_by = None;
        if runtime.observed == ObservedState::Failed && !runtime.has_process {
            runtime.observed = ObservedState::Inactive;
        }
        Ok(())
    }

    /// Requests activation of a service or group and returns immediate effects.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::UnknownTarget`] when the target is absent from the
    /// current snapshot.
    pub fn start(
        &mut self,
        target: &ServiceId,
        now_ms: u64,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let activation = self
            .snapshot
            .activation_services(target)
            .ok_or_else(|| RuntimeError::UnknownTarget(target.clone()))?;
        for service in activation {
            let Some(runtime) = self.services.get_mut(&service) else {
                continue;
            };
            runtime.desired = DesiredState::Active;
            runtime.restart_blocked = false;
            runtime.blocked_by = None;
            if runtime.observed == ObservedState::Failed && !runtime.has_process {
                runtime.observed = ObservedState::Inactive;
            }
        }
        Ok(self.schedule(now_ms))
    }

    /// Requests ordered deactivation of a service or group.
    ///
    /// Stopping a service also stops active services that require it. Stopping a
    /// group deactivates the group's complete activation closure.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::UnknownTarget`] when the target is absent.
    pub fn stop(&mut self, target: &ServiceId) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let mut stopping = if self.snapshot.groups().contains_key(target) {
            self.snapshot
                .activation_services(target)
                .ok_or_else(|| RuntimeError::UnknownTarget(target.clone()))?
        } else if self.snapshot.services().contains_key(target) {
            BTreeSet::from([target.clone()])
        } else {
            return Err(RuntimeError::UnknownTarget(target.clone()));
        };

        loop {
            let previous_len = stopping.len();
            for service in self.snapshot.services().keys() {
                if self
                    .required_services(service)
                    .iter()
                    .any(|required| stopping.contains(required))
                {
                    stopping.insert(service.clone());
                }
            }
            if stopping.len() == previous_len {
                break;
            }
        }

        for service in stopping {
            let Some(runtime) = self.services.get_mut(&service) else {
                continue;
            };
            runtime.desired = DesiredState::Inactive;
            runtime.restart_blocked = false;
            runtime.waiting_restart = false;
            runtime.blocked_by = None;
        }
        Ok(self.schedule_stops())
    }

    /// Requests a complete stop followed by a fresh start of one service.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is unknown or names a group.
    pub fn restart(
        &mut self,
        service: &ServiceId,
        now_ms: u64,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let stop_timeout_ms = self
            .snapshot
            .services()
            .get(service)
            .map(|definition| definition.supervision.stop_timeout_ms)
            .unwrap_or_default();
        let runtime = self.services.get_mut(service).ok_or_else(|| {
            if self.snapshot.groups().contains_key(service) {
                RuntimeError::NotService(service.clone())
            } else {
                RuntimeError::UnknownTarget(service.clone())
            }
        })?;
        runtime.desired = DesiredState::Active;
        runtime.restart_blocked = false;
        runtime.blocked_by = None;

        if runtime.has_process {
            runtime.observed = ObservedState::Stopping;
            runtime.stop_result = StopResult::Restart;
            Ok(terminate_effects(service, runtime, false, stop_timeout_ms))
        } else {
            runtime.observed = ObservedState::Inactive;
            Ok(self.schedule(now_ms))
        }
    }

    #[must_use]
    pub fn handle(&mut self, event: RuntimeEvent) -> Vec<RuntimeEffect> {
        let now_ms = match &event {
            RuntimeEvent::ExecSucceeded { at_ms, .. }
            | RuntimeEvent::Ready { at_ms, .. }
            | RuntimeEvent::SpawnFailed { at_ms, .. }
            | RuntimeEvent::ReadinessFailed { at_ms, .. }
            | RuntimeEvent::Exited { at_ms, .. }
            | RuntimeEvent::Timer { at_ms, .. } => *at_ms,
        };
        let mut effects = Vec::new();
        match event {
            RuntimeEvent::ExecSucceeded {
                service,
                generation,
                ..
            } => self.exec_succeeded(&service, generation),
            RuntimeEvent::Ready {
                service,
                generation,
                ..
            } => self.ready(&service, generation),
            RuntimeEvent::SpawnFailed {
                service,
                generation,
                at_ms,
            } => self.spawn_failed(&service, generation, at_ms, &mut effects),
            RuntimeEvent::ReadinessFailed {
                service,
                generation,
                ..
            } => self.readiness_failed(&service, generation, &mut effects),
            RuntimeEvent::Exited {
                service,
                generation,
                outcome,
                at_ms,
            } => self.exited(&service, generation, outcome, at_ms, &mut effects),
            RuntimeEvent::Timer {
                service,
                generation,
                kind,
                at_ms,
            } => self.timer(&service, generation, kind, at_ms, &mut effects),
        }
        effects.extend(self.propagate_dependency_failures());
        effects.extend(self.schedule_stops());
        effects.extend(self.schedule(now_ms));
        effects
    }

    fn exec_succeeded(&mut self, service: &ServiceId, generation: u64) {
        let readiness = self.snapshot.services()[service].process.readiness;
        let Some(runtime) = self.current_attempt_mut(service, generation) else {
            return;
        };
        if runtime.observed == ObservedState::Starting && readiness == Readiness::Exec {
            runtime.observed = ObservedState::Active;
            runtime.blocked_by = None;
        }
    }

    fn ready(&mut self, service: &ServiceId, generation: u64) {
        let readiness = self
            .snapshot
            .services()
            .get(service)
            .map(|definition| definition.process.readiness);
        let Some(runtime) = self.current_attempt_mut(service, generation) else {
            return;
        };
        if runtime.observed == ObservedState::Starting && readiness == Some(Readiness::Notify) {
            runtime.observed = ObservedState::Active;
            runtime.blocked_by = None;
        }
    }

    fn spawn_failed(
        &mut self,
        service: &ServiceId,
        generation: u64,
        at_ms: u64,
        effects: &mut Vec<RuntimeEffect>,
    ) {
        let Some(runtime) = self.current_attempt_mut(service, generation) else {
            return;
        };
        runtime.has_process = false;
        self.finish_unexpected_exit(service, ExitOutcome::ExitCode(127), at_ms, effects);
    }

    fn readiness_failed(
        &mut self,
        service: &ServiceId,
        generation: u64,
        effects: &mut Vec<RuntimeEffect>,
    ) {
        let stop_timeout_ms = self
            .snapshot
            .services()
            .get(service)
            .map(|definition| definition.supervision.stop_timeout_ms)
            .unwrap_or_default();
        let Some(runtime) = self.current_attempt_mut(service, generation) else {
            return;
        };
        if runtime.observed == ObservedState::Starting {
            runtime.observed = ObservedState::Stopping;
            runtime.stop_result = StopResult::RestartFailure;
            effects.extend(terminate_effects(service, runtime, false, stop_timeout_ms));
        }
    }

    fn exited(
        &mut self,
        service: &ServiceId,
        generation: u64,
        outcome: ExitOutcome,
        at_ms: u64,
        effects: &mut Vec<RuntimeEffect>,
    ) {
        let process_kind = self.snapshot.services()[service].process.kind;
        let Some(runtime) = self.current_attempt_mut(service, generation) else {
            return;
        };
        runtime.has_process = false;

        if runtime.observed == ObservedState::Stopping {
            let result = runtime.stop_result;
            runtime.stop_result = StopResult::Inactive;
            match result {
                StopResult::Inactive | StopResult::Restart | StopResult::RestartFailure => {
                    runtime.observed = ObservedState::Inactive;
                }
                StopResult::Failed => runtime.observed = ObservedState::Failed,
            }
            if result == StopResult::RestartFailure {
                self.finish_unexpected_exit(service, ExitOutcome::ExitCode(124), at_ms, effects);
            }
            return;
        }

        if process_kind == ProcessType::Oneshot
            && runtime.observed == ObservedState::Starting
            && outcome == ExitOutcome::Success
        {
            runtime.observed = ObservedState::Active;
            runtime.restart_history.clear();
            runtime.backoff_ms = INITIAL_RESTART_BACKOFF_MS;
            return;
        }
        self.finish_unexpected_exit(service, outcome, at_ms, effects);
    }

    fn finish_unexpected_exit(
        &mut self,
        service: &ServiceId,
        outcome: ExitOutcome,
        at_ms: u64,
        effects: &mut Vec<RuntimeEffect>,
    ) {
        let definition = &self.snapshot.services()[service];
        let runtime = self
            .services
            .get_mut(service)
            .expect("event service has runtime");
        let failed = outcome.failed() || runtime.observed == ObservedState::Starting;
        let should_restart = runtime.desired == DesiredState::Active
            && match definition.supervision.restart {
                RestartPolicy::No => false,
                RestartPolicy::OnFailure => failed,
                RestartPolicy::Always => true,
            };

        if should_restart && register_restart(runtime, definition, at_ms) {
            runtime.observed = ObservedState::Starting;
            runtime.waiting_restart = true;
            effects.push(RuntimeEffect::ArmTimer {
                service: service.clone(),
                generation: runtime.generation,
                kind: TimerKind::Restart,
                after_ms: runtime.backoff_ms,
            });
            runtime.backoff_ms = runtime
                .backoff_ms
                .saturating_mul(2)
                .min(definition.supervision.restart_backoff_max_ms);
        } else {
            runtime.observed = if failed {
                ObservedState::Failed
            } else {
                ObservedState::Inactive
            };
            runtime.restart_blocked = true;
            runtime.waiting_restart = false;
        }
    }

    fn timer(
        &mut self,
        service: &ServiceId,
        generation: u64,
        kind: TimerKind,
        _at_ms: u64,
        effects: &mut Vec<RuntimeEffect>,
    ) {
        let stop_timeout_ms = self
            .snapshot
            .services()
            .get(service)
            .map(|definition| definition.supervision.stop_timeout_ms)
            .unwrap_or_default();
        let Some(runtime) = self.services.get_mut(service) else {
            return;
        };
        if runtime.generation != generation {
            return;
        }
        match kind {
            TimerKind::Start
                if runtime.observed == ObservedState::Starting && runtime.has_process =>
            {
                runtime.observed = ObservedState::Stopping;
                runtime.stop_result = StopResult::RestartFailure;
                effects.extend(terminate_effects(service, runtime, false, stop_timeout_ms));
            }
            TimerKind::Stop if runtime.observed == ObservedState::Stopping => {
                effects.push(RuntimeEffect::Terminate {
                    service: service.clone(),
                    generation,
                    force: true,
                });
            }
            TimerKind::Restart if runtime.waiting_restart => {
                runtime.waiting_restart = false;
                runtime.observed = ObservedState::Inactive;
            }
            _ => {}
        }
    }

    fn current_attempt_mut(
        &mut self,
        service: &ServiceId,
        generation: u64,
    ) -> Option<&mut ServiceRuntime> {
        self.services
            .get_mut(service)
            .filter(|runtime| runtime.generation == generation && runtime.has_process)
    }

    fn schedule(&mut self, now_ms: u64) -> Vec<RuntimeEffect> {
        self.recover_available_dependencies();
        let mut effects = Vec::new();
        let mut available = self.snapshot.max_starting().saturating_sub(
            self.services
                .values()
                .filter(|runtime| {
                    runtime.observed == ObservedState::Starting && runtime.has_process
                })
                .count(),
        );
        if available == 0 {
            return effects;
        }

        let candidates = self.services.keys().cloned().collect::<Vec<_>>();
        for service in candidates {
            if available == 0 {
                break;
            }
            let runtime = &self.services[&service];
            if runtime.desired != DesiredState::Active
                || runtime.observed != ObservedState::Inactive
                || runtime.restart_blocked
                || runtime.waiting_restart
            {
                continue;
            }
            let required = self.required_services(&service);
            if let Some(failed) = required.iter().find(|required| {
                self.services
                    .get(*required)
                    .is_some_and(|state| state.observed == ObservedState::Failed)
            }) {
                let runtime = self.services.get_mut(&service).expect("known service");
                runtime.observed = ObservedState::Failed;
                runtime.blocked_by = Some(failed.clone());
                continue;
            }
            if required.iter().any(|required| {
                self.services
                    .get(required)
                    .is_none_or(|state| state.observed != ObservedState::Active)
            }) || !self.ordering_ready(&service)
            {
                continue;
            }

            let definition = &self.snapshot.services()[&service];
            let runtime = self.services.get_mut(&service).expect("known service");
            runtime.generation = runtime.generation.wrapping_add(1).max(1);
            runtime.observed = ObservedState::Starting;
            runtime.has_process = true;
            runtime.started_at_ms = now_ms;
            runtime.blocked_by = None;
            effects.push(RuntimeEffect::Spawn {
                service: service.clone(),
                generation: runtime.generation,
            });
            if definition.supervision.start_timeout_ms > 0 {
                effects.push(RuntimeEffect::ArmTimer {
                    service,
                    generation: runtime.generation,
                    kind: TimerKind::Start,
                    after_ms: definition.supervision.start_timeout_ms,
                });
            }
            available -= 1;
        }
        effects
    }

    fn schedule_stops(&mut self) -> Vec<RuntimeEffect> {
        let mut effects = Vec::new();
        let candidates = self.services.keys().cloned().collect::<Vec<_>>();
        for service in candidates {
            let runtime = &self.services[&service];
            if runtime.desired != DesiredState::Inactive
                || matches!(
                    runtime.observed,
                    ObservedState::Inactive | ObservedState::Stopping
                )
            {
                continue;
            }
            if self.has_stopping_dependent(&service) {
                continue;
            }
            let stop_timeout_ms = self.snapshot.services()[&service]
                .supervision
                .stop_timeout_ms;
            let runtime = self.services.get_mut(&service).expect("known service");
            if runtime.has_process {
                runtime.observed = ObservedState::Stopping;
                runtime.stop_result = StopResult::Inactive;
                effects.extend(terminate_effects(&service, runtime, false, stop_timeout_ms));
            } else {
                runtime.observed = ObservedState::Inactive;
            }
        }
        effects
    }

    fn propagate_dependency_failures(&mut self) -> Vec<RuntimeEffect> {
        let mut effects = Vec::new();
        let candidates = self.services.keys().cloned().collect::<Vec<_>>();
        for service in candidates {
            let failed = self
                .required_services(&service)
                .into_iter()
                .find(|required| {
                    self.services
                        .get(required)
                        .is_some_and(|runtime| runtime.observed != ObservedState::Active)
                });
            let Some(failed) = failed else {
                continue;
            };
            let runtime = self.services.get_mut(&service).expect("known service");
            if runtime.desired != DesiredState::Active || runtime.blocked_by.is_some() {
                continue;
            }
            let stop_timeout_ms = self.snapshot.services()[&service]
                .supervision
                .stop_timeout_ms;
            runtime.blocked_by = Some(failed);
            if runtime.has_process {
                runtime.observed = ObservedState::Stopping;
                runtime.stop_result = StopResult::Failed;
                effects.extend(terminate_effects(&service, runtime, false, stop_timeout_ms));
            } else {
                runtime.observed = ObservedState::Failed;
            }
        }
        effects
    }

    fn recover_available_dependencies(&mut self) {
        let recovered = self
            .services
            .iter()
            .filter_map(|(service, runtime)| {
                let blocked = runtime.blocked_by.as_ref()?;
                let dependency = self.services.get(blocked)?;
                (dependency.observed == ObservedState::Active
                    && !runtime.has_process
                    && runtime.desired == DesiredState::Active)
                    .then(|| service.clone())
            })
            .collect::<Vec<_>>();
        for service in recovered {
            if let Some(runtime) = self.services.get_mut(&service) {
                runtime.blocked_by = None;
                runtime.observed = ObservedState::Inactive;
            }
        }
    }

    fn required_services(&self, service: &ServiceId) -> BTreeSet<ServiceId> {
        let mut required = BTreeSet::new();
        let mut pending = self.snapshot.services()[service]
            .dependencies
            .requires
            .iter()
            .cloned()
            .collect::<VecDeque<_>>();
        let mut visited = BTreeSet::new();
        while let Some(target) = pending.pop_front() {
            if !visited.insert(target.clone()) {
                continue;
            }
            if self.snapshot.services().contains_key(&target) {
                required.insert(target.clone());
            }
            if let Some(dependencies) = self.snapshot.dependencies(&target) {
                pending.extend(dependencies.requires.iter().cloned());
                if self.snapshot.groups().contains_key(&target) {
                    pending.extend(
                        dependencies
                            .wants
                            .iter()
                            .filter(|wanted| self.snapshot.dependencies(wanted).is_some())
                            .cloned(),
                    );
                }
            }
        }
        required
    }

    fn ordering_ready(&self, service: &ServiceId) -> bool {
        self.snapshot.services()[service]
            .dependencies
            .after
            .iter()
            .all(|target| {
                let Some(ordered) = self.snapshot.activation_services(target) else {
                    return true;
                };
                ordered.iter().all(|dependency| {
                    let runtime = &self.services[dependency];
                    runtime.desired == DesiredState::Inactive
                        || matches!(
                            runtime.observed,
                            ObservedState::Active | ObservedState::Failed
                        )
                })
            })
    }

    fn has_stopping_dependent(&self, required: &ServiceId) -> bool {
        self.services.iter().any(|(service, runtime)| {
            service != required
                && runtime.desired == DesiredState::Inactive
                && !matches!(
                    runtime.observed,
                    ObservedState::Inactive | ObservedState::Failed
                )
                && self.required_services(service).contains(required)
        })
    }
}

#[derive(Clone, Debug)]
struct ServiceRuntime {
    desired: DesiredState,
    observed: ObservedState,
    generation: u64,
    has_process: bool,
    waiting_restart: bool,
    restart_blocked: bool,
    blocked_by: Option<ServiceId>,
    stop_result: StopResult,
    started_at_ms: u64,
    restart_history: VecDeque<u64>,
    backoff_ms: u64,
}

impl Default for ServiceRuntime {
    fn default() -> Self {
        Self {
            desired: DesiredState::Inactive,
            observed: ObservedState::Inactive,
            generation: 0,
            has_process: false,
            waiting_restart: false,
            restart_blocked: false,
            blocked_by: None,
            stop_result: StopResult::Inactive,
            started_at_ms: 0,
            restart_history: VecDeque::new(),
            backoff_ms: INITIAL_RESTART_BACKOFF_MS,
        }
    }
}

impl ServiceRuntime {
    fn view(&self) -> ServiceStatus {
        ServiceStatus {
            desired: self.desired,
            observed: self.observed,
            generation: self.generation,
            blocked_by: self.blocked_by.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopResult {
    Inactive,
    Failed,
    Restart,
    RestartFailure,
}

fn terminate_effects(
    service: &ServiceId,
    runtime: &ServiceRuntime,
    force: bool,
    stop_timeout_ms: u64,
) -> Vec<RuntimeEffect> {
    let mut effects = vec![RuntimeEffect::Terminate {
        service: service.clone(),
        generation: runtime.generation,
        force,
    }];
    if !force && stop_timeout_ms > 0 {
        effects.push(RuntimeEffect::ArmTimer {
            service: service.clone(),
            generation: runtime.generation,
            kind: TimerKind::Stop,
            after_ms: stop_timeout_ms,
        });
    }
    effects
}

fn register_restart(
    runtime: &mut ServiceRuntime,
    definition: &crate::model::ServiceDefinition,
    at_ms: u64,
) -> bool {
    if at_ms.saturating_sub(runtime.started_at_ms) >= definition.supervision.restart_reset_ms {
        runtime.restart_history.clear();
        runtime.backoff_ms = INITIAL_RESTART_BACKOFF_MS;
    }
    let window_start = at_ms.saturating_sub(definition.supervision.restart_window_ms);
    while runtime
        .restart_history
        .front()
        .is_some_and(|timestamp| *timestamp < window_start)
    {
        runtime.restart_history.pop_front();
    }
    if runtime.restart_history.len()
        >= usize::try_from(definition.supervision.restart_limit).unwrap_or(usize::MAX)
    {
        return false;
    }
    runtime.restart_history.push_back(at_ms);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConfigSnapshot, model::ManagerScope};

    fn snapshot(service_sources: &[(&str, &str)], wants: &[&str]) -> Arc<ConfigSnapshot> {
        let wanted = wants
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manager = format!(
            "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = [{wanted}]\n"
        );
        let ids = service_sources
            .iter()
            .map(|(name, source)| (ServiceId::new(*name).unwrap(), *source));
        Arc::new(ConfigSnapshot::build(&manager, ids, ManagerScope::System).unwrap())
    }

    fn simple(extra: &str) -> String {
        format!("schema_version = 1\n[process]\ncommand = [\"/usr/bin/true\"]\n{extra}\n")
    }

    fn spawns(effects: &[RuntimeEffect], name: &str) -> Option<u64> {
        effects.iter().find_map(|effect| match effect {
            RuntimeEffect::Spawn {
                service,
                generation,
            } if service.as_str() == name => Some(*generation),
            _ => None,
        })
    }

    #[test]
    fn starts_all_graph_ready_services_concurrently() {
        let a = simple("");
        let b = simple("");
        let config = snapshot(&[("a", &a), ("b", &b)], &["a", "b"]);
        let mut engine = RuntimeEngine::new(config);

        let effects = engine.start(&ServiceId::new("boot").unwrap(), 10).unwrap();
        assert!(spawns(&effects, "a").is_some());
        assert!(spawns(&effects, "b").is_some());
    }

    #[test]
    fn required_service_gates_dependant_readiness() {
        let db = simple("");
        let web = simple("[dependencies]\nrequires = [\"db\"]");
        let config = snapshot(&[("db", &db), ("web", &web)], &["web"]);
        let mut engine = RuntimeEngine::new(config);

        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let generation = spawns(&effects, "db").unwrap();
        assert!(spawns(&effects, "web").is_none());

        let effects = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("db").unwrap(),
            generation,
            at_ms: 1,
        });
        assert!(spawns(&effects, "web").is_some());
    }

    #[test]
    fn ignores_stale_attempt_events() {
        let service = simple("");
        let config = snapshot(&[("svc", &service)], &["svc"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let generation = spawns(&effects, "svc").unwrap();

        let _ = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("svc").unwrap(),
            generation: generation + 1,
            at_ms: 1,
        });
        assert_eq!(
            engine
                .status(&ServiceId::new("svc").unwrap())
                .unwrap()
                .observed,
            ObservedState::Starting
        );
    }

    #[test]
    fn oneshot_becomes_active_only_after_successful_exit() {
        let service = simple("type = \"oneshot\"");
        let config = snapshot(&[("mount", &service)], &["mount"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let generation = spawns(&effects, "mount").unwrap();

        let _ = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("mount").unwrap(),
            generation,
            at_ms: 1,
        });
        assert_eq!(
            engine
                .status(&ServiceId::new("mount").unwrap())
                .unwrap()
                .observed,
            ObservedState::Starting
        );
        let _ = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("mount").unwrap(),
            generation,
            outcome: ExitOutcome::Success,
            at_ms: 1,
        });
        assert_eq!(
            engine
                .status(&ServiceId::new("mount").unwrap())
                .unwrap()
                .observed,
            ObservedState::Active
        );
    }

    #[test]
    fn restarts_with_backoff_and_stops_at_limit() {
        let service = simple(
            "[supervision]\nrestart = \"on-failure\"\nrestart_limit = 1\nrestart_window_ms = 1000",
        );
        let config = snapshot(&[("svc", &service)], &["svc"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let generation = spawns(&effects, "svc").unwrap();

        let effects = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("svc").unwrap(),
            generation,
            outcome: ExitOutcome::ExitCode(1),
            at_ms: 10,
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ArmTimer {
                kind: TimerKind::Restart,
                after_ms: 100,
                ..
            }
        )));

        let effects = engine.handle(RuntimeEvent::Timer {
            service: ServiceId::new("svc").unwrap(),
            generation,
            kind: TimerKind::Restart,
            at_ms: 110,
        });
        let next = spawns(&effects, "svc").unwrap();
        let _ = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("svc").unwrap(),
            generation: next,
            outcome: ExitOutcome::ExitCode(1),
            at_ms: 120,
        });
        assert_eq!(
            engine
                .status(&ServiceId::new("svc").unwrap())
                .unwrap()
                .observed,
            ObservedState::Failed
        );
    }

    #[test]
    fn start_timeout_uses_restart_policy_and_configured_stop_timeout() {
        let service = simple("[supervision]\nrestart = \"on-failure\"\nstop_timeout_ms = 123");
        let config = snapshot(&[("svc", &service)], &["svc"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let generation = spawns(&effects, "svc").unwrap();

        let effects = engine.handle(RuntimeEvent::Timer {
            service: ServiceId::new("svc").unwrap(),
            generation,
            kind: TimerKind::Start,
            at_ms: 30_000,
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ArmTimer {
                kind: TimerKind::Stop,
                after_ms: 123,
                ..
            }
        )));

        let effects = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("svc").unwrap(),
            generation,
            outcome: ExitOutcome::Signal(15),
            at_ms: 30_001,
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ArmTimer {
                kind: TimerKind::Restart,
                ..
            }
        )));
    }

    #[test]
    fn required_service_loss_stops_and_later_recovers_dependant() {
        let db = simple("[supervision]\nrestart = \"on-failure\"\nrestart_limit = 2");
        let web = simple("[dependencies]\nrequires = [\"db\"]");
        let config = snapshot(&[("db", &db), ("web", &web)], &["web"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let db_generation = spawns(&effects, "db").unwrap();
        let effects = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("db").unwrap(),
            generation: db_generation,
            at_ms: 1,
        });
        let web_generation = spawns(&effects, "web").unwrap();
        let _ = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("web").unwrap(),
            generation: web_generation,
            at_ms: 2,
        });

        let effects = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("db").unwrap(),
            generation: db_generation,
            outcome: ExitOutcome::ExitCode(1),
            at_ms: 3,
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Terminate { service, .. } if service.as_str() == "web"
        )));

        let _ = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("web").unwrap(),
            generation: web_generation,
            outcome: ExitOutcome::Signal(15),
            at_ms: 4,
        });
        let effects = engine.handle(RuntimeEvent::Timer {
            service: ServiceId::new("db").unwrap(),
            generation: db_generation,
            kind: TimerKind::Restart,
            at_ms: 103,
        });
        let next_db = spawns(&effects, "db").unwrap();
        let effects = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("db").unwrap(),
            generation: next_db,
            at_ms: 104,
        });
        assert!(spawns(&effects, "web").is_some());
    }

    #[test]
    fn stops_dependants_before_required_service() {
        let db = simple("");
        let web = simple("[dependencies]\nrequires = [\"db\"]");
        let config = snapshot(&[("db", &db), ("web", &web)], &["web"]);
        let mut engine = RuntimeEngine::new(config);
        let effects = engine.start(&ServiceId::new("boot").unwrap(), 0).unwrap();
        let db_generation = spawns(&effects, "db").unwrap();
        let effects = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("db").unwrap(),
            generation: db_generation,
            at_ms: 1,
        });
        let web_generation = spawns(&effects, "web").unwrap();
        let _ = engine.handle(RuntimeEvent::ExecSucceeded {
            service: ServiceId::new("web").unwrap(),
            generation: web_generation,
            at_ms: 2,
        });

        let effects = engine.stop(&ServiceId::new("db").unwrap()).unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Terminate { service, .. } if service.as_str() == "web"
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Terminate { service, .. } if service.as_str() == "db"
        )));

        let effects = engine.handle(RuntimeEvent::Exited {
            service: ServiceId::new("web").unwrap(),
            generation: web_generation,
            outcome: ExitOutcome::Success,
            at_ms: 10,
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Terminate { service, .. } if service.as_str() == "db"
        )));
    }
}
