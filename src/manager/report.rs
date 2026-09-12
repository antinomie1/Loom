// SPDX-License-Identifier: BSD-2-Clause

use super::{
    Manager, MessageKind, Operation, Packet, ServiceId, StatusCode, critical_path_for,
    desired_name, observed_name,
};
use serde::Serialize;
use std::{collections::BTreeMap, io, os::fd::AsFd};

#[derive(Default, Serialize)]
pub(super) struct Report {
    pub(super) schema_version: u32,
    pub(super) status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) manager_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rescue_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) services: Option<Vec<ServiceReport>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) dependencies: Option<DependencyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) critical_path: Option<CriticalPathReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) plan: Option<crate::runtime::ApplyPlan>,
}

#[derive(Serialize)]
pub(super) struct ServiceReport {
    name: String,
    state: &'static str,
    desired: &'static str,
    generation: u64,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exited_ms: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct DependencyReport {
    requires: Vec<String>,
    wants: Vec<String>,
    after: Vec<String>,
    conflicts: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct CriticalPathReport {
    duration_ms: u64,
    services: Vec<String>,
}

impl Manager {
    pub(super) fn query_report(&self, operation: Operation, target: &[u8]) -> Report {
        let target = std::str::from_utf8(target).unwrap_or("").trim();
        let mut report = Report::default();
        match operation {
            Operation::Status
            | Operation::List
            | Operation::Timings
            | Operation::IsActive
            | Operation::IsEnabled => {
                report.services = Some(
                    self.engine
                        .statuses()
                        .filter(|(id, _)| target.is_empty() || id.as_str() == target)
                        .map(|(id, status)| ServiceReport {
                            name: id.to_string(),
                            state: observed_name(status.observed),
                            desired: desired_name(status.desired),
                            generation: status.generation,
                            enabled: self.engine.snapshot().is_enabled(id),
                            blocked_by: status.blocked_by.map(|id| id.to_string()),
                            queued_ms: status.queued_at_ms,
                            started_ms: status.started_at_ms,
                            ready_ms: status.ready_at_ms,
                            exited_ms: status.exited_at_ms,
                        })
                        .collect(),
                );
                if target.is_empty() {
                    report.manager_pid = Some(std::process::id());
                    let rescue = self.rescue.as_ref().filter(|state| !state.recovering);
                    report.mode = Some(if rescue.is_some() {
                        "rescue"
                    } else {
                        "running"
                    });
                    report.rescue_reason = rescue.map(|state| state.reason.clone());
                }
            }
            Operation::Dependencies => {
                if let Ok(id) = ServiceId::new(target)
                    && let Some(dependencies) = self.engine.snapshot().dependencies(&id)
                {
                    let names = |ids: &[ServiceId]| ids.iter().map(ToString::to_string).collect();
                    report.dependencies = Some(DependencyReport {
                        requires: names(&dependencies.requires),
                        wants: names(&dependencies.wants),
                        after: names(&dependencies.after),
                        conflicts: names(&dependencies.conflicts),
                    });
                }
            }
            Operation::CriticalPath => {
                let statuses = self
                    .engine
                    .statuses()
                    .map(|(id, status)| (id.clone(), status))
                    .collect::<BTreeMap<_, _>>();
                let mut memo = BTreeMap::new();
                let (duration_ms, services) = statuses
                    .iter()
                    .filter(|(_, status)| status.ready_at_ms.is_some())
                    .map(|(id, _)| {
                        critical_path_for(id, self.engine.snapshot(), &statuses, &mut memo)
                    })
                    .max_by_key(|(duration, _)| *duration)
                    .unwrap_or_default();
                report.critical_path = Some(CriticalPathReport {
                    duration_ms,
                    services: services.iter().map(ToString::to_string).collect(),
                });
            }
            _ => {}
        }
        report
    }

    pub(super) fn respond_report(
        &mut self,
        token: u64,
        request_id: u64,
        operation: Operation,
        status: StatusCode,
        mut report: Report,
    ) {
        report.schema_version = 1;
        report.status = status as u16;
        match toml_edit::ser::to_string(&report) {
            Ok(payload) => {
                self.queue_response(token, request_id, operation, status, payload.as_bytes());
            }
            Err(_) => self.remove_client(token),
        }
    }

    pub(super) fn queue_response(
        &mut self,
        token: u64,
        request_id: u64,
        operation: Operation,
        status: StatusCode,
        payload: &[u8],
    ) {
        const MAX_RESPONSE: usize = 8 * 1024 * 1024;
        let queued = self
            .clients
            .values()
            .flat_map(|client| &client.outgoing)
            .map(Vec::len)
            .sum::<usize>();
        if payload.len() > MAX_RESPONSE || queued.saturating_add(payload.len()) > 64 * 1024 * 1024 {
            self.remove_client(token);
            return;
        }
        let chunks = payload
            .len()
            .max(1)
            .div_ceil(crate::protocol::MAX_PAYLOAD_LEN);
        for index in 0..chunks {
            let start = index * crate::protocol::MAX_PAYLOAD_LEN;
            let end = payload.len().min(start + crate::protocol::MAX_PAYLOAD_LEN);
            let packet = Packet {
                kind: MessageKind::Response,
                request_id,
                operation,
                status,
                more: index + 1 < chunks,
                payload: payload[start..end].to_vec(),
            };
            let Ok(packet) = packet.encode() else {
                self.remove_client(token);
                return;
            };
            let Some(client) = self.clients.get_mut(&token) else {
                return;
            };
            client.outgoing.push_back(packet);
        }
        self.flush_client(token);
    }

    pub(super) fn flush_client(&mut self, token: u64) {
        let Some(client) = self.clients.get_mut(&token) else {
            return;
        };
        while let Some(packet) = client.outgoing.front() {
            match client.connection.send(packet) {
                Ok(()) => {
                    client.outgoing.pop_front();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.remove_client(token);
                    return;
                }
            }
        }
        if self
            .reactor
            .modify(
                client.connection.as_fd(),
                token,
                !client.outgoing.is_empty(),
            )
            .is_err()
        {
            self.remove_client(token);
        }
    }
}
