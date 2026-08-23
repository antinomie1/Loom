// SPDX-License-Identifier: BSD-2-Clause
#![allow(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
};

use thiserror::Error;

use crate::model::{OutputTarget, Readiness, ResolvedIdentity, ServiceDefinition};

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to prepare {stream}: {source}")]
    PrepareIo {
        stream: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to create readiness socket: {0}")]
    NotifySocket(io::Error),
    #[error("failed to spawn process: {0}")]
    Spawn(io::Error),
    #[error("pidfd_open failed for PID {pid}: {source}")]
    Pidfd { pid: u32, source: io::Error },
    #[error("PID {0} does not fit Linux pid_t")]
    InvalidPid(u32),
    #[error("failed to signal process group {pid}: {source}")]
    Signal { pid: u32, source: io::Error },
    #[error("failed to wait for PID {pid}: {source}")]
    Wait { pid: u32, source: io::Error },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationRead {
    Pending,
    Message(Vec<u8>),
    Closed,
}

pub struct SpawnedProcess {
    child: Child,
    pidfd: OwnedFd,
    notification: Option<OwnedFd>,
}

impl SpawnedProcess {
    /// Starts one foreground service and opens a pidfd before returning.
    ///
    /// `identity = None` preserves the manager's credentials, which is the safe
    /// choice for a non-root user manager. The system manager passes a resolved
    /// numeric identity.
    ///
    /// # Errors
    ///
    /// Returns an error when IO setup, child setup/exec, or pidfd creation fails.
    /// A child created before pidfd failure is killed and reaped before return.
    pub fn spawn(
        definition: &ServiceDefinition,
        identity: Option<&ResolvedIdentity>,
        base_environment: &BTreeMap<String, String>,
    ) -> Result<Self, ProcessError> {
        let mut command = Command::new(&definition.process.command[0]);
        command.args(&definition.process.command[1..]);
        command.current_dir(&definition.process.working_directory);
        command.env_clear();
        command.envs(base_environment);
        command.envs(&definition.process.environment);
        command.process_group(0);
        command.stdin(Stdio::null());
        command.stdout(output_stdio(&definition.io.stdout).map_err(|source| {
            ProcessError::PrepareIo {
                stream: "stdout",
                source,
            }
        })?);
        command.stderr(output_stdio(&definition.io.stderr).map_err(|source| {
            ProcessError::PrepareIo {
                stream: "stderr",
                source,
            }
        })?);

        let (notification, child_notification) =
            if definition.process.readiness == Readiness::Notify {
                let (parent, child) = seqpacket_pair().map_err(ProcessError::NotifySocket)?;
                command.env("LOOM_NOTIFY_FD", child.as_raw_fd().to_string());
                (Some(parent), Some(child))
            } else {
                (None, None)
            };

        let child_notification_fd = child_notification.as_ref().map(AsRawFd::as_raw_fd);
        let identity = identity.cloned();
        let umask = definition.process.umask;
        // SAFETY: the closure performs only async-signal-safe libc operations,
        // captures owned scalar/vector data prepared before fork, and reports
        // failures through `io::Error` for Command's exec-error pipe.
        unsafe {
            command.pre_exec(move || {
                if child_notification_fd.is_some_and(|fd| libc::fcntl(fd, libc::F_SETFD, 0) == -1) {
                    return Err(io::Error::last_os_error());
                }
                libc::umask(umask as libc::mode_t);
                if let Some(identity) = &identity {
                    let group_count = identity.supplementary_groups.len();
                    let groups = if group_count == 0 {
                        std::ptr::null()
                    } else {
                        identity.supplementary_groups.as_ptr().cast::<libc::gid_t>()
                    };
                    if libc::setgroups(group_count, groups) == -1
                        || libc::setgid(identity.gid) == -1
                        || libc::setuid(identity.uid) == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(ProcessError::Spawn)?;
        drop(child_notification);
        let pid = child.id();
        let pidfd = match open_pidfd(pid) {
            Ok(pidfd) => pidfd,
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::Pidfd { pid, source });
            }
        };

        Ok(Self {
            child,
            pidfd,
            notification,
        })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    #[must_use]
    pub const fn pidfd(&self) -> &OwnedFd {
        &self.pidfd
    }

    #[must_use]
    pub fn notification_fd(&self) -> Option<&OwnedFd> {
        self.notification.as_ref()
    }

    /// Receives one bounded readiness packet without blocking.
    ///
    /// # Errors
    ///
    /// Returns an IO error other than `EAGAIN`/`EWOULDBLOCK` from `recv`.
    pub fn read_notification(&self) -> io::Result<NotificationRead> {
        let Some(socket) = &self.notification else {
            return Ok(NotificationRead::Closed);
        };
        let mut buffer = [0_u8; 4096];
        // SAFETY: `buffer` is writable for its full reported length and `socket`
        // owns a valid SOCK_SEQPACKET descriptor for this call.
        let received = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if received > 0 {
            let length = usize::try_from(received)
                .map_err(|_| io::Error::other("recv length does not fit usize"))?;
            return Ok(NotificationRead::Message(buffer[..length].to_vec()));
        }
        if received == 0 {
            return Ok(NotificationRead::Closed);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(NotificationRead::Pending)
        } else {
            Err(error)
        }
    }

    /// Sends SIGTERM or SIGKILL to the process group created for this service.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid PID or a failed `kill` other than ESRCH.
    pub fn terminate(&self, force: bool) -> Result<(), ProcessError> {
        let pid = i32::try_from(self.pid()).map_err(|_| ProcessError::InvalidPid(self.pid()))?;
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: `kill` receives a valid negative process-group identifier and
        // a constant signal number. No pointer lifetimes are involved.
        let result = unsafe { libc::kill(-pid, signal) };
        if result == 0 {
            return Ok(());
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(ProcessError::Signal {
                pid: self.pid(),
                source,
            })
        }
    }

    /// Polls the child without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when `waitpid(WNOHANG)` fails.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError> {
        let pid = self.pid();
        self.child
            .try_wait()
            .map_err(|source| ProcessError::Wait { pid, source })
    }

    /// Reaps the child, blocking until it exits.
    ///
    /// # Errors
    ///
    /// Returns an error when `waitpid` fails.
    pub fn wait(&mut self) -> Result<ExitStatus, ProcessError> {
        let pid = self.pid();
        self.child
            .wait()
            .map_err(|source| ProcessError::Wait { pid, source })
    }
}

fn output_stdio(target: &OutputTarget) -> io::Result<Stdio> {
    match target {
        OutputTarget::Console => Ok(Stdio::inherit()),
        OutputTarget::Null => Ok(Stdio::null()),
        OutputTarget::Append(path) => OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(path)
            .map(Stdio::from),
        OutputTarget::Socket(path) => UnixStream::connect(path)
            .map(OwnedFd::from)
            .map(Stdio::from),
    }
}

fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    // SAFETY: `descriptors` points to two writable integers. On success both are
    // newly owned descriptors; on failure neither descriptor is transferred.
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            descriptors.as_mut_ptr(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful socketpair returned two distinct, newly owned FDs.
    let parent = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    // SAFETY: ownership of the second distinct descriptor is transferred once.
    let child = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    Ok((parent, child))
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds pid_t"))?;
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned FD on
    // success. The PID remains allocated because its Child has not been reaped.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let fd =
        i32::try_from(fd).map_err(|_| io::Error::other("pidfd does not fit a file descriptor"))?;
    // SAFETY: a successful pidfd_open returns one newly owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use std::{path::Path, thread, time::Duration};

    use crate::model::{ManagerScope, ServiceId};

    use super::*;

    fn definition(command: &[&str], readiness: &str) -> ServiceDefinition {
        let command = command
            .iter()
            .map(|argument| format!("\"{argument}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            "schema_version = 1\n[process]\ncommand = [{command}]\nreadiness = \"{readiness}\"\n[io]\nstdout = \"null\"\nstderr = \"null\"\n"
        );
        ServiceDefinition::parse(ServiceId::new("test").unwrap(), &source, ManagerScope::User)
            .unwrap()
    }

    #[test]
    fn opens_pidfd_and_reaps_process() {
        let service = definition(&["/bin/sh", "-c", "exit 7"], "exec");
        let mut process = SpawnedProcess::spawn(&service, None, &BTreeMap::new()).unwrap();

        assert!(process.pidfd().as_raw_fd() >= 0);
        let status = process.wait().unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn terminates_whole_service_process_group() {
        let service = definition(&["/bin/sleep", "30"], "exec");
        let mut process = SpawnedProcess::spawn(&service, None, &BTreeMap::new()).unwrap();

        process.terminate(true).unwrap();
        let status = process.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn passes_nonblocking_notify_socket() {
        let service = definition(
            &["/bin/sh", "-c", "eval 'printf READY >&'$LOOM_NOTIFY_FD"],
            "notify",
        );
        let mut process = SpawnedProcess::spawn(&service, None, &BTreeMap::new()).unwrap();

        let mut notification = NotificationRead::Pending;
        for _ in 0..100 {
            notification = process.read_notification().unwrap();
            if notification != NotificationRead::Pending {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = process.wait().unwrap();
        assert_eq!(notification, NotificationRead::Message(b"READY".to_vec()));
    }

    #[test]
    fn requires_absolute_executable_before_spawn() {
        let source = "schema_version = 1\n[process]\ncommand = [\"true\"]\n";
        assert!(
            ServiceDefinition::parse(ServiceId::new("test").unwrap(), source, ManagerScope::User,)
                .is_err()
        );
        assert!(Path::new("/bin/true").is_absolute());
    }
}
