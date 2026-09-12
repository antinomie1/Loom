// SPDX-License-Identifier: BSD-2-Clause

use super::{
    CgroupDomain, DeadlineAction, Manager, ManagerError, ProcessSlot, Source, SpawnedProcess,
    monotonic_ms, process_io,
};
use std::os::fd::{AsFd, AsRawFd};

pub(super) struct RescueState {
    pub(super) reason: String,
    pub(super) process: Option<ProcessSlot>,
    pub(super) recovering: bool,
}

impl Manager {
    pub(super) fn enter_rescue(&mut self, reason: String) -> Result<(), ManagerError> {
        if self.rescue.is_some() || self.shutdown.is_some() {
            return Ok(());
        }
        eprintln!("loom: entering rescue mode: {reason}");
        self.rescue = Some(RescueState {
            reason,
            process: None,
            recovering: false,
        });
        self.start_rescue()
    }

    pub(super) fn start_rescue(&mut self) -> Result<(), ManagerError> {
        if self
            .rescue
            .as_ref()
            .is_none_or(|state| state.recovering || state.process.is_some())
            || self.shutdown.is_some()
        {
            return Ok(());
        }
        let token = self.allocate_token();
        let result = (|| -> Result<ProcessSlot, ManagerError> {
            let cgroup = self
                .cgroup_root
                .as_deref()
                .map(|root| CgroupDomain::create(root, "rescue", token))
                .transpose()?;
            let mut process = SpawnedProcess::spawn_rescue(
                self.engine.snapshot().rescue_command(),
                cgroup.as_ref(),
            )
            .map_err(process_io)?;
            if let Err(error) = self.reactor.add(process.pidfd().as_fd(), token, false) {
                let _ = process.terminate(true);
                let _ = process.wait();
                return Err(error.into());
            }
            let domain_token = match self.register_domain(cgroup.as_ref(), Source::Rescue) {
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
            self.sources.insert(token, Source::Rescue);
            Ok(ProcessSlot {
                process,
                generation: token,
                pid_token: token,
                notify_token: None,
                domain_token,
                cgroup,
                exit_status: None,
            })
        })();
        match result {
            Ok(process) => self.rescue.as_mut().expect("rescue is active").process = Some(process),
            Err(error) => {
                eprintln!("loom: cannot start rescue command: {error}");
                self.push_deadline(monotonic_ms()?.saturating_add(1000), DeadlineAction::Rescue)?;
            }
        }
        Ok(())
    }

    pub(super) fn poll_rescue(&mut self) -> Result<(), ManagerError> {
        let Some(state) = &mut self.rescue else {
            return Ok(());
        };
        if let Some(process) = &mut state.process {
            if !process.poll_exit(&self.reactor)? {
                return Ok(());
            }
            process.unregister(&self.reactor, &mut self.sources);
            state.process = None;
            if !state.recovering && self.shutdown.is_none() {
                self.push_deadline(monotonic_ms()?.saturating_add(1000), DeadlineAction::Rescue)?;
                return Ok(());
            }
        }
        if state.recovering {
            self.rescue = None;
        }
        Ok(())
    }

    pub(super) fn recover_rescue(&mut self) -> Result<(), ManagerError> {
        let Some(state) = &mut self.rescue else {
            return Ok(());
        };
        state.recovering = true;
        if let Some(process) = &state.process {
            process.signal(true)?;
        }
        self.poll_rescue()
    }
}
