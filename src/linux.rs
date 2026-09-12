// SPDX-License-Identifier: BSD-2-Clause
#![allow(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsString},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{FileExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

use thiserror::Error;

pub mod reactor;

use crate::model::{OutputTarget, Readiness, ResolvedIdentity, ServiceDefinition};

/// Returns the calling process's real UID, GID, and supplementary groups.
///
/// # Errors
///
/// Returns an error when Linux rejects either `getgroups` call.
pub fn current_identity() -> io::Result<ResolvedIdentity> {
    // SAFETY: getuid/getgid take no pointers and have no failure return.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // SAFETY: a zero-length getgroups call accepts a null output pointer.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let count =
        usize::try_from(count).map_err(|_| io::Error::other("group count does not fit usize"))?;
    let mut supplementary_groups = vec![0_u32; count];
    if count > 0 {
        // SAFETY: the vector is writable for `count` gid_t values and Linux
        // gid_t is u32 on every supported architecture.
        let result = unsafe {
            libc::getgroups(
                i32::try_from(count)
                    .map_err(|_| io::Error::other("group count does not fit i32"))?,
                supplementary_groups.as_mut_ptr().cast::<libc::gid_t>(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    supplementary_groups.sort_unstable();
    supplementary_groups.dedup();
    Ok(ResolvedIdentity {
        uid,
        gid,
        supplementary_groups,
    })
}

/// Reaps one exited child without blocking. This is the PID-1 orphan fallback;
/// normally tracked children are reaped through their pidfds first.
///
/// # Errors
///
/// Returns an unexpected `waitpid` error.
pub fn reap_exited_child() -> io::Result<Option<i32>> {
    let mut status = 0;
    // SAFETY: waitpid writes one integer and WNOHANG prevents blocking.
    let pid = unsafe { libc::waitpid(-1, std::ptr::addr_of_mut!(status), libc::WNOHANG) };
    match pid.cmp(&0) {
        std::cmp::Ordering::Greater => Ok(Some(pid)),
        std::cmp::Ordering::Equal => Ok(None),
        std::cmp::Ordering::Less => {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// Makes this manager adopt descendants left behind by service leaders.
///
/// # Errors
/// Returns the error from `prctl`.
pub fn become_subreaper() -> io::Result<()> {
    // SAFETY: prctl receives a documented operation and scalar arguments.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reaps adopted children without consuming the exit status of a tracked child.
///
/// # Errors
/// Returns an unexpected procfs or waitpid error.
pub fn reap_untracked_children(tracked: &BTreeSet<u32>) -> io::Result<()> {
    let children =
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
    for child in children.split_whitespace() {
        let pid: u32 = child
            .parse()
            .map_err(|_| io::Error::other("invalid child PID"))?;
        if tracked.contains(&pid) {
            continue;
        }
        let pid = i32::try_from(pid).map_err(|_| io::Error::other("invalid child PID"))?;
        // SAFETY: waitpid has no output pointer and never blocks.
        if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == -1 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::ECHILD | libc::EINTR)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Flushes filesystem buffers and requests a Linux reboot or poweroff.
///
/// # Errors
///
/// Returns the Linux error when the reboot syscall returns.
pub fn shutdown_system(reboot: bool) -> io::Result<()> {
    // SAFETY: sync has no arguments and only flushes kernel filesystem buffers.
    unsafe { libc::sync() };
    let command = if reboot {
        libc::LINUX_REBOOT_CMD_RESTART
    } else {
        libc::LINUX_REBOOT_CMD_POWER_OFF
    };
    // SAFETY: reboot receives one documented constant and requires privilege;
    // successful calls do not return.
    if unsafe { libc::reboot(command) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Mounts the kernel API filesystems required by the system manager.
/// Existing mounts of the expected filesystem type are left untouched.
///
/// # Errors
///
/// Returns the first directory, `statfs`, or mount failure.
pub fn mount_api_filesystems() -> io::Result<()> {
    if !Path::new("/dev/null").exists() {
        mount_api("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, -1, None)?;
    }
    mount_api(
        "proc",
        "/proc",
        "proc",
        libc::MS_NOSUID | libc::MS_NODEV,
        0x9fa0,
        None,
    )?;
    mount_api(
        "sysfs",
        "/sys",
        "sysfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        0x6265_6572,
        None,
    )?;
    mount_api(
        "tmpfs",
        "/run",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        0x0102_1994,
        Some("mode=0755"),
    )?;
    std::fs::create_dir_all("/sys/fs/cgroup")?;
    mount_api(
        "none",
        "/sys/fs/cgroup",
        "cgroup2",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        0x6367_7270,
        None,
    )
}

fn mount_api(
    source: &str,
    target: &str,
    filesystem: &str,
    flags: libc::c_ulong,
    magic: libc::c_long,
    data: Option<&str>,
) -> io::Result<()> {
    std::fs::create_dir_all(target)?;
    let target_c = CString::new(target).expect("static mount target has no NUL");
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: target is a valid NUL-terminated path and status is writable.
    if unsafe { libc::statfs(target_c.as_ptr(), status.as_mut_ptr()) } == 0 {
        // SAFETY: statfs initialized status after returning success.
        if unsafe { status.assume_init() }.f_type == magic {
            return Ok(());
        }
    }
    let source = CString::new(source).expect("static mount source has no NUL");
    let filesystem = CString::new(filesystem).expect("static filesystem name has no NUL");
    let data = data.map(|value| CString::new(value).expect("static mount data has no NUL"));
    let data_ptr = data
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr().cast());
    // SAFETY: all strings are NUL-terminated, flags are mount(2) flags, and
    // the optional data remains alive for the syscall.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            filesystem.as_ptr(),
            flags,
            data_ptr,
        )
    } == -1
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EBUSY) {
            return Err(io::Error::new(
                error.kind(),
                format!("mount {filesystem:?} at {target}: {error}"),
            ));
        }
    }
    Ok(())
}

/// Runs an interactive rescue shell forever. Intended only for PID 1 after a
/// fatal initialization failure, so PID 1 never exits and panics the kernel.
pub fn rescue_loop(reason: &str) -> ! {
    eprintln!("loom: entering rescue mode: {reason}");
    if let Err(error) = clear_signal_mask() {
        eprintln!("loom: cannot clear rescue signal mask: {error}");
    }
    loop {
        match Command::new("/bin/sh")
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .spawn()
        {
            Ok(mut shell) => {
                let _ = shell.wait();
                eprintln!("loom: rescue shell exited; restarting");
            }
            Err(error) => eprintln!("loom: cannot start /bin/sh: {error}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// One cgroup v2 process domain owned by a service attempt.
fn clear_signal_mask() -> io::Result<()> {
    let mut empty_mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: empty_mask is writable and becomes initialized before it is
    // passed as the input set to sigprocmask.
    if unsafe { libc::sigemptyset(empty_mask.as_mut_ptr()) } == -1
        || unsafe {
            libc::sigprocmask(libc::SIG_SETMASK, empty_mask.as_ptr(), std::ptr::null_mut())
        } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub struct CgroupDomain {
    path: PathBuf,
    procs: File,
    events: File,
}

impl CgroupDomain {
    /// Creates a process domain below a manager-owned cgroup.
    ///
    /// # Errors
    ///
    /// Returns an error from directory creation or opening `cgroup.procs`.
    pub fn create(root: &Path, service: &str, generation: u64) -> io::Result<Self> {
        let path = root.join(format!("{service}-{generation}"));
        std::fs::create_dir(&path)?;
        let result = (|| -> io::Result<Self> {
            let procs = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(path.join("cgroup.procs"))?;
            let events = File::open(path.join("cgroup.events"))?;
            Ok(Self {
                path: path.clone(),
                procs,
                events,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir(&path);
        }
        result
    }

    #[must_use]
    fn procs_fd(&self) -> &File {
        &self.procs
    }

    #[must_use]
    pub const fn events(&self) -> &File {
        &self.events
    }

    /// Checks the kernel's recursive populated state, acknowledging notifications.
    ///
    /// # Errors
    /// Returns an IO error or rejects a missing populated record.
    pub fn is_empty(&self) -> io::Result<bool> {
        let mut buffer = [0_u8; 4096];
        let length = self.events.read_at(&mut buffer, 0)?;
        let events = std::str::from_utf8(&buffer[..length])
            .map_err(|_| io::Error::other("invalid cgroup.events"))?;
        match events
            .lines()
            .find_map(|line| line.strip_prefix("populated "))
        {
            Some("0") => Ok(true),
            Some("1") => Ok(false),
            _ => Err(io::Error::other("missing cgroup populated state")),
        }
    }

    /// Signals every task in the process domain.
    ///
    /// # Errors
    ///
    /// Returns a cgroup read/write, PID parse, or signal failure.
    pub fn terminate(&self, force: bool) -> io::Result<()> {
        let kill_path = self.path.join("cgroup.kill");
        if force && kill_path.exists() {
            return std::fs::write(kill_path, b"1");
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let mut paths = vec![self.path.clone()];
        while let Some(path) = paths.pop() {
            paths.extend(cgroup_children(&path)?);
            for line in std::fs::read_to_string(path.join("cgroup.procs"))?.lines() {
                let pid = line.parse::<i32>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid cgroup PID")
                })?;
                // SAFETY: PID came from cgroup.procs and signal is a valid constant.
                if unsafe { libc::kill(pid, signal) } == -1 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for CgroupDomain {
    fn drop(&mut self) {
        let _ = remove_empty_cgroups(&self.path);
    }
}

fn cgroup_children(path: &Path) -> io::Result<Vec<PathBuf>> {
    std::fs::read_dir(path)?
        .filter_map(|entry| match entry {
            Ok(entry) => match entry.file_type() {
                Ok(kind) if kind.is_dir() => Some(Ok(entry.path())),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            },
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn remove_empty_cgroups(path: &Path) -> io::Result<()> {
    for child in cgroup_children(path)? {
        remove_empty_cgroups(&child)?;
    }
    std::fs::remove_dir(path)
}

/// Creates the manager's cgroup v2 subtree. A user manager succeeds only when
/// its current cgroup has been delegated by the session owner.
///
/// # Errors
///
/// Returns an error when cgroup v2 is unavailable or child creation is denied.
pub fn prepare_cgroup_root(system: bool, uid: u32) -> io::Result<PathBuf> {
    let mount = Path::new("/sys/fs/cgroup");
    if !mount.join("cgroup.controllers").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cgroup v2 is not mounted",
        ));
    }
    let relative = if system {
        PathBuf::new()
    } else {
        let membership = std::fs::read_to_string("/proc/self/cgroup")?;
        let entry = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing cgroup v2 entry"))?;
        PathBuf::from(entry.trim_start_matches('/'))
    };
    let name = if system {
        "loom".to_owned()
    } else {
        format!("loom-user-{uid}")
    };
    let root = mount.join(relative).join(name);
    std::fs::create_dir_all(&root)?;
    for entry in std::fs::read_dir(&root)? {
        let path = entry?.path();
        if path.is_dir() {
            let _ = std::fs::write(path.join("cgroup.kill"), b"1");
            let _ = remove_empty_cgroups(&path);
        }
    }
    Ok(root)
}

pub struct StartupLock {
    _file: File,
}

impl StartupLock {
    /// Takes an exclusive advisory lock used to serialize user-manager startup.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be securely opened or locked.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        // SAFETY: the file owns a valid descriptor and flock takes scalar flags.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

pub struct UserManagerChild {
    child: Child,
    readiness: OwnedFd,
}

impl UserManagerChild {
    /// Waits for the manager's explicit readiness message and kills failed startups.
    ///
    /// # Errors
    /// Returns timeout, early exit, or malformed readiness errors.
    pub fn wait_ready(mut self, timeout: Duration) -> io::Result<()> {
        let mut poll = libc::pollfd {
            fd: self.readiness.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let deadline = std::time::Instant::now() + timeout;
        let result = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "user manager startup timed out",
                ));
            }
            let milliseconds = i32::try_from(remaining.as_millis())
                .unwrap_or(i32::MAX)
                .max(1);
            // SAFETY: poll points to one initialized entry and timeout is bounded.
            let ready = unsafe { libc::poll(std::ptr::addr_of_mut!(poll), 1, milliseconds) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break Err(error);
            }
            if ready == 0 {
                continue;
            }
            let mut byte = 0_u8;
            // SAFETY: readiness is an owned pipe and byte has one writable byte.
            let count = unsafe {
                libc::read(
                    self.readiness.as_raw_fd(),
                    std::ptr::addr_of_mut!(byte).cast(),
                    1,
                )
            };
            break if count == 1 && byte == b'R' {
                Ok(())
            } else {
                Err(io::Error::other("user manager exited before readiness"))
            };
        };
        if result.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        result
    }
}

/// Starts a manager in a new session with a one-shot readiness pipe.
///
/// # Errors
/// Returns pipe creation, process creation, or child setup errors.
pub fn spawn_user_manager(program: &Path, arguments: &[OsString]) -> io::Result<UserManagerChild> {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors has room for both newly allocated descriptors.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned distinct new descriptors, each transferred once.
    let readiness = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    // SAFETY: the second pipe end is independently owned.
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let fd = writer.as_raw_fd();
    let mut command = Command::new(program);
    command
        .args(arguments)
        .arg("--ready-fd")
        .arg(fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: setup uses async-signal-safe calls and only scalar captured values.
    unsafe {
        command.pre_exec(move || {
            clear_signal_mask()?;
            if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(writer);
    Ok(UserManagerChild { child, readiness })
}

/// Acknowledges completed manager initialization to the launcher.
///
/// # Errors
/// Returns an invalid descriptor or pipe write error.
pub fn notify_launcher(fd: i32) -> io::Result<()> {
    if fd < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid readiness descriptor",
        ));
    }
    // SAFETY: write reads one initialized byte; the caller designated this FD.
    let written = unsafe { libc::write(fd, b"R".as_ptr().cast(), 1) };
    let error = io::Error::last_os_error();
    // SAFETY: this inherited descriptor is consumed once by the readiness handshake.
    unsafe { libc::close(fd) };
    if written == 1 { Ok(()) } else { Err(error) }
}

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
        cgroup: Option<&CgroupDomain>,
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
        let cgroup_fd = cgroup.map(|domain| domain.procs_fd().as_raw_fd());
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
                clear_signal_mask()?;
                if let Some(fd) = cgroup_fd {
                    const SELF_CGROUP: &[u8] = b"0";
                    if libc::write(fd, SELF_CGROUP.as_ptr().cast(), SELF_CGROUP.len()) != 1 {
                        return Err(io::Error::last_os_error());
                    }
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

    /// Starts an interactive rescue command in its own session on the console.
    ///
    /// # Errors
    /// Returns an error opening the console, creating the child or its pidfd.
    pub fn spawn_rescue(
        arguments: &[String],
        cgroup: Option<&CgroupDomain>,
    ) -> Result<Self, ProcessError> {
        let mut command = Command::new(&arguments[0]);
        command
            .args(&arguments[1..])
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .env("TERM", "linux");
        let console = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/console")
            .map_err(|source| ProcessError::PrepareIo {
                stream: "console",
                source,
            })?;
        command
            .stdin(console.try_clone().map_err(ProcessError::Spawn)?)
            .stdout(console.try_clone().map_err(ProcessError::Spawn)?)
            .stderr(console);
        let cgroup_fd = cgroup.map(|domain| domain.procs_fd().as_raw_fd());
        // SAFETY: all captured state is scalar; child setup uses only libc syscalls.
        unsafe {
            command.pre_exec(move || {
                clear_signal_mask()?;
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::isatty(0) == 1 && libc::ioctl(0, libc::TIOCSCTTY, 1) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(fd) = cgroup_fd
                    && libc::write(fd, b"0".as_ptr().cast(), 1) != 1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(ProcessError::Spawn)?;
        let pid = child.id();
        let pidfd = match open_pidfd(pid) {
            Ok(fd) => fd,
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::Pidfd { pid, source });
            }
        };
        Ok(Self {
            child,
            pidfd,
            notification: None,
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

    pub fn take_notification(&mut self) -> Option<OwnedFd> {
        self.notification.take()
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

    /// Checks whether the fallback process group still contains processes.
    ///
    /// # Errors
    /// Returns an unexpected kill error.
    pub fn group_is_empty(&self) -> io::Result<bool> {
        let pid = i32::try_from(self.pid()).map_err(|_| io::Error::other("invalid PID"))?;
        // SAFETY: signal zero only probes existence of the service process group.
        if unsafe { libc::kill(-pid, 0) } == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            Some(libc::EPERM) => Ok(false),
            _ => Err(error),
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
        let mut process = SpawnedProcess::spawn(&service, None, &BTreeMap::new(), None).unwrap();

        assert!(process.pidfd().as_raw_fd() >= 0);
        let status = process.wait().unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn terminates_whole_service_process_group() {
        let service = definition(&["/bin/sleep", "30"], "exec");
        let mut process = SpawnedProcess::spawn(&service, None, &BTreeMap::new(), None).unwrap();

        process.terminate(true).unwrap();
        let status = process.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn passes_nonblocking_notify_socket() {
        // Other tests can release low-numbered descriptors after we reserve
        // ours. Run this descriptor-sensitive check in a fresh test process.
        if std::env::var_os("LOOM_TEST_NOTIFY_ISOLATED").is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "linux::tests::passes_nonblocking_notify_socket",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env("LOOM_TEST_NOTIFY_ISOLATED", "1")
                .status()
                .unwrap();
            assert!(status.success(), "isolated notify test failed");
            return;
        }
        // Keep the notification descriptor above 9: POSIX shell redirections
        // are not a portable way to write to arbitrary inherited descriptors.
        let _held = (0..16)
            .map(|_| File::open("/dev/null").unwrap())
            .collect::<Vec<_>>();
        let executable = std::env::current_exe().unwrap();
        let mut service = definition(
            &[
                executable.to_str().unwrap(),
                "--exact",
                "linux::tests::notify_child_helper",
                "--test-threads=1",
                "--nocapture",
            ],
            "notify",
        );
        service.io.stderr = OutputTarget::Console;
        let environment = BTreeMap::from([("LOOM_TEST_NOTIFY_CHILD".into(), "1".into())]);
        let mut process = SpawnedProcess::spawn(&service, None, &environment, None).unwrap();

        let mut notification = NotificationRead::Pending;
        for _ in 0..100 {
            notification = process.read_notification().unwrap();
            if notification != NotificationRead::Pending {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(process.wait().unwrap().success(), "notify helper failed");
        assert_eq!(notification, NotificationRead::Message(b"READY".to_vec()));
    }

    #[test]
    fn notify_child_helper() {
        if std::env::var_os("LOOM_TEST_NOTIFY_CHILD").is_none() {
            return;
        }
        let fd: i32 = std::env::var("LOOM_NOTIFY_FD").unwrap().parse().unwrap();
        assert!(fd > 9);
        // SAFETY: the parent passed a notification socket and READY has five bytes.
        assert_eq!(unsafe { libc::write(fd, b"READY".as_ptr().cast(), 5) }, 5);
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
