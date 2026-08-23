// SPDX-License-Identifier: BSD-2-Clause
#![allow(unsafe_code)]

use std::{
    ffi::CString,
    fs, io,
    mem::{size_of, zeroed},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

use crate::protocol::MAX_PACKET_LEN;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadyEvent {
    pub token: u64,
    pub readable: bool,
    pub writable: bool,
    pub hangup: bool,
}

pub struct Reactor {
    epoll: OwnedFd,
}

impl Reactor {
    /// Creates an epoll instance owned by this reactor.
    ///
    /// # Errors
    ///
    /// Returns the Linux error from `epoll_create1`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: epoll_create1 takes scalar flags and returns a new descriptor.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        Ok(Self {
            epoll: owned_fd(fd)?,
        })
    }

    /// Registers a descriptor with a stable caller-provided token.
    ///
    /// # Errors
    ///
    /// Returns the Linux error from `epoll_ctl(ADD)`.
    pub fn add(&self, fd: BorrowedFd<'_>, token: u64, writable: bool) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_ADD, fd.as_raw_fd(), token, writable)
    }

    /// Changes writable interest without changing the token.
    ///
    /// # Errors
    ///
    /// Returns the Linux error from `epoll_ctl(MOD)`.
    pub fn modify(&self, fd: BorrowedFd<'_>, token: u64, writable: bool) -> io::Result<()> {
        self.control(libc::EPOLL_CTL_MOD, fd.as_raw_fd(), token, writable)
    }

    /// Removes a descriptor. Removing an already closed/unregistered descriptor
    /// is idempotent.
    ///
    /// # Errors
    ///
    /// Returns unexpected Linux errors from `epoll_ctl(DEL)`.
    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: the epoll descriptor is owned and the target FD is passed only
        // as an integer; DEL ignores the event pointer.
        let result = unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::EBADF)) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// Waits for a bounded batch of readiness events.
    ///
    /// # Errors
    ///
    /// Returns an error from `epoll_wait`, except EINTR which is retried.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<Vec<ReadyEvent>> {
        let timeout_ms = timeout.map_or(-1, |duration| {
            i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
        });
        let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
        loop {
            // SAFETY: `events` is writable for its full capacity and epoll_wait
            // initializes exactly the number of entries it returns.
            let count = unsafe {
                libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), 64, timeout_ms)
            };
            if count >= 0 {
                let count = usize::try_from(count)
                    .map_err(|_| io::Error::other("epoll count does not fit usize"))?;
                events.truncate(count);
                return Ok(events
                    .into_iter()
                    .map(|event| ReadyEvent {
                        token: event.u64,
                        readable: event.events & libc::EPOLLIN as u32 != 0,
                        writable: event.events & libc::EPOLLOUT as u32 != 0,
                        hangup: event.events
                            & (libc::EPOLLHUP | libc::EPOLLRDHUP | libc::EPOLLERR) as u32
                            != 0,
                    })
                    .collect());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn control(&self, operation: i32, fd: RawFd, token: u64, writable: bool) -> io::Result<()> {
        let mut flags = (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLERR) as u32;
        if writable {
            flags |= libc::EPOLLOUT as u32;
        }
        let mut event = libc::epoll_event {
            events: flags,
            u64: token,
        };
        // SAFETY: both descriptors are integers and `event` is initialized for
        // the duration of epoll_ctl.
        let result = unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                operation,
                fd,
                std::ptr::addr_of_mut!(event),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub struct TimerFd(OwnedFd);

impl TimerFd {
    /// Creates a nonblocking monotonic timer.
    ///
    /// # Errors
    ///
    /// Returns the Linux error from `timerfd_create`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: timerfd_create takes scalar constants and returns a new FD.
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        owned_fd(fd).map(Self)
    }

    /// Arms a one-shot timer. Zero duration is rounded up to one nanosecond.
    ///
    /// # Errors
    ///
    /// Returns the Linux error from `timerfd_settime`.
    pub fn arm(&self, after: Duration) -> io::Result<()> {
        let seconds = i64::try_from(after.as_secs()).unwrap_or(i64::MAX);
        let nanoseconds = if after.is_zero() {
            1
        } else {
            i64::from(after.subsec_nanos())
        };
        let specification = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
        };
        // SAFETY: `specification` is initialized and lives for this call.
        let result = unsafe {
            libc::timerfd_settime(
                self.0.as_raw_fd(),
                0,
                std::ptr::addr_of!(specification),
                std::ptr::null_mut(),
            )
        };
        cvt(result)
    }

    /// Consumes the current expiration count.
    ///
    /// # Errors
    ///
    /// Returns an unexpected error from `read`; a drained timer returns zero.
    pub fn consume(&self) -> io::Result<u64> {
        let mut expirations = 0_u64;
        // SAFETY: the destination points to one writable u64, timerfd reads are
        // exactly eight bytes, and this descriptor is nonblocking.
        let result = unsafe {
            libc::read(
                self.0.as_raw_fd(),
                std::ptr::addr_of_mut!(expirations).cast(),
                size_of::<u64>(),
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(error);
        }
        Ok(expirations)
    }
}

impl AsFd for TimerFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

pub struct SignalFd(OwnedFd);

impl SignalFd {
    /// Blocks the selected signals on the calling thread and exposes them as a
    /// nonblocking descriptor. Call before creating worker threads.
    ///
    /// # Errors
    ///
    /// Returns an error from sigset construction, `pthread_sigmask`, or
    /// `signalfd`.
    pub fn new(signals: &[i32]) -> io::Result<Self> {
        // SAFETY: sigset_t is a C plain-old-data value initialized immediately
        // by sigemptyset before use.
        let mut mask = unsafe { zeroed::<libc::sigset_t>() };
        // SAFETY: `mask` points to writable sigset storage.
        if unsafe { libc::sigemptyset(std::ptr::addr_of_mut!(mask)) } == -1 {
            return Err(io::Error::last_os_error());
        }
        for signal in signals {
            // SAFETY: mask remains initialized and signals are caller-provided
            // scalar values validated by sigaddset.
            if unsafe { libc::sigaddset(std::ptr::addr_of_mut!(mask), *signal) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: mask is initialized; null old-mask means no output pointer.
        let result = unsafe {
            libc::pthread_sigmask(
                libc::SIG_BLOCK,
                std::ptr::addr_of!(mask),
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: signalfd copies the initialized mask and returns a new FD.
        let fd = unsafe {
            libc::signalfd(
                -1,
                std::ptr::addr_of!(mask),
                libc::SFD_NONBLOCK | libc::SFD_CLOEXEC,
            )
        };
        owned_fd(fd).map(Self)
    }

    /// Reads one queued signal number, or `None` when drained.
    ///
    /// # Errors
    ///
    /// Returns an unexpected error or short record from `read`.
    pub fn read_signal(&self) -> io::Result<Option<i32>> {
        // SAFETY: signalfd_siginfo is plain data initialized by a complete read
        // before any field is observed.
        let mut information = unsafe { zeroed::<libc::signalfd_siginfo>() };
        // SAFETY: destination is writable for exactly one complete record.
        let result = unsafe {
            libc::read(
                self.0.as_raw_fd(),
                std::ptr::addr_of_mut!(information).cast(),
                size_of::<libc::signalfd_siginfo>(),
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        if usize::try_from(result).ok() != Some(size_of::<libc::signalfd_siginfo>()) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short signalfd record",
            ));
        }
        i32::try_from(information.ssi_signo)
            .map(Some)
            .map_err(|_| io::Error::other("signal number does not fit i32"))
    }
}

impl AsFd for SignalFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

pub struct SeqPacketListener {
    fd: OwnedFd,
    path: PathBuf,
}

impl SeqPacketListener {
    /// Binds a nonblocking Unix seqpacket listener, replacing only a stale socket
    /// owned by `expected_uid`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe existing paths, oversized/non-UTF-8 paths, or
    /// socket/bind/listen/permission failures.
    pub fn bind(path: &Path, mode: u32, expected_uid: u32) -> io::Result<Self> {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if !metadata.file_type().is_socket() || metadata.uid() != expected_uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to replace unsafe control socket path",
                ));
            }
            fs::remove_file(path)?;
        }
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path contains NUL",
            ));
        }
        // SAFETY: sockaddr_un is plain data and every observed field is written
        // below before bind.
        let mut address = unsafe { zeroed::<libc::sockaddr_un>() };
        address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
            .map_err(|_| io::Error::other("AF_UNIX does not fit sa_family_t"))?;
        if bytes.len() >= address.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path is too long",
            ));
        }
        for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
            *destination = i8::from_ne_bytes([*source]);
        }
        let length = libc::socklen_t::try_from(size_of::<libc::sa_family_t>() + bytes.len() + 1)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path is too long"))?;
        // SAFETY: socket returns a new descriptor or -1.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        let fd = owned_fd(fd)?;
        // SAFETY: address and length describe the initialized pathname socket.
        if unsafe { libc::bind(fd.as_raw_fd(), std::ptr::addr_of!(address).cast(), length) } == -1 {
            return Err(io::Error::last_os_error());
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        // SAFETY: fd is a bound SOCK_SEQPACKET descriptor.
        if unsafe { libc::listen(fd.as_raw_fd(), 128) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    /// Accepts one connection, or returns `None` when the queue is drained.
    ///
    /// # Errors
    ///
    /// Returns an unexpected error from `accept4`.
    pub fn accept(&self) -> io::Result<Option<SeqPacketConnection>> {
        // SAFETY: null address pointers request no peer pathname; accept4 returns
        // a new descriptor with the selected flags.
        let fd = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            return owned_fd(fd).map(SeqPacketConnection).map(Some);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

impl AsFd for SeqPacketListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for SeqPacketListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct SeqPacketConnection(OwnedFd);

impl SeqPacketConnection {
    /// Connects to an existing Unix seqpacket listener.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths or socket/connect failure.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        let path = CString::new(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
        // SAFETY: sockaddr_un is initialized before connect.
        let mut address = unsafe { zeroed::<libc::sockaddr_un>() };
        address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
            .map_err(|_| io::Error::other("AF_UNIX does not fit sa_family_t"))?;
        let bytes = path.as_bytes();
        if bytes.len() >= address.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path is too long",
            ));
        }
        for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
            *destination = i8::from_ne_bytes([*source]);
        }
        let length = libc::socklen_t::try_from(size_of::<libc::sa_family_t>() + bytes.len() + 1)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path is too long"))?;
        // SAFETY: socket returns a new descriptor or -1.
        let fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        let fd = owned_fd(fd)?;
        // SAFETY: address and length are initialized for this pathname.
        if unsafe { libc::connect(fd.as_raw_fd(), std::ptr::addr_of!(address).cast(), length) }
            == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    /// Receives one complete packet, or `None` when drained/closed.
    ///
    /// # Errors
    ///
    /// Returns an unexpected recv error or rejects an oversized packet.
    pub fn receive(&self) -> io::Result<Option<Vec<u8>>> {
        let mut packet = vec![0_u8; MAX_PACKET_LEN];
        // SAFETY: packet is writable for its length; MSG_TRUNC reports the full
        // seqpacket length so truncation is detectable.
        let result = unsafe {
            libc::recv(
                self.0.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                libc::MSG_TRUNC,
            )
        };
        if result == 0 {
            return Ok(None);
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        let length = usize::try_from(result)
            .map_err(|_| io::Error::other("packet length does not fit usize"))?;
        if length > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control packet exceeds hard limit",
            ));
        }
        packet.truncate(length);
        Ok(Some(packet))
    }

    /// Sends one complete packet.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized packet, backpressure, or failed send.
    pub fn send(&self, packet: &[u8]) -> io::Result<()> {
        if packet.len() > MAX_PACKET_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control packet exceeds hard limit",
            ));
        }
        // SAFETY: packet points to initialized bytes for the complete call.
        let result = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(result).ok() == Some(packet.len()) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short seqpacket send",
            ))
        }
    }

    /// Reads Linux peer credentials fixed at connection time.
    ///
    /// # Errors
    ///
    /// Returns an error from `getsockopt(SO_PEERCRED)` or an unexpected length.
    pub fn peer_credentials(&self) -> io::Result<PeerCredentials> {
        // SAFETY: ucred is plain data initialized by getsockopt before use.
        let mut credentials = unsafe { zeroed::<libc::ucred>() };
        let mut length = libc::socklen_t::try_from(size_of::<libc::ucred>())
            .map_err(|_| io::Error::other("ucred size does not fit socklen_t"))?;
        // SAFETY: both output pointers are valid for their declared sizes.
        if unsafe {
            libc::getsockopt(
                self.0.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(credentials).cast(),
                std::ptr::addr_of_mut!(length),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(length).ok() != Some(size_of::<libc::ucred>()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid SO_PEERCRED length",
            ));
        }
        Ok(PeerCredentials {
            pid: credentials.pid,
            uid: credentials.uid,
            gid: credentials.gid,
        })
    }
}

impl AsFd for SeqPacketConnection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

fn owned_fd(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: callers pass only a newly returned, nonnegative descriptor and
        // transfer ownership exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn cvt(result: i32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::{os::fd::AsFd, process, thread};

    use super::*;

    #[test]
    fn reactor_observes_timer() {
        let reactor = Reactor::new().unwrap();
        let timer = TimerFd::new().unwrap();
        reactor.add(timer.as_fd(), 7, false).unwrap();
        timer.arm(Duration::from_millis(1)).unwrap();

        let events = reactor.wait(Some(Duration::from_secs(1))).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.token == 7 && event.readable)
        );
        assert_eq!(timer.consume().unwrap(), 1);
    }

    #[test]
    fn seqpacket_preserves_message_and_peer_credentials() {
        let directory = std::env::temp_dir().join(format!("loom-reactor-{}", process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        let uid = fs::metadata(&directory).unwrap().uid();
        let path = directory.join("control.sock");
        let listener = SeqPacketListener::bind(&path, 0o600, uid).unwrap();
        let client = SeqPacketConnection::connect(&path).unwrap();
        let server = loop {
            if let Some(connection) = listener.accept().unwrap() {
                break connection;
            }
            thread::yield_now();
        };

        client.send(b"one packet").unwrap();
        assert_eq!(server.receive().unwrap(), Some(b"one packet".to_vec()));
        assert_eq!(server.peer_credentials().unwrap().uid, uid);
        drop(listener);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn signalfd_receives_blocked_thread_signal() {
        let signals = SignalFd::new(&[libc::SIGUSR2]).unwrap();
        // SAFETY: raise sends a valid signal to the calling thread, where it is
        // blocked and therefore queued for the signalfd.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR2) }, 0);
        for _ in 0..100 {
            if let Some(signal) = signals.read_signal().unwrap() {
                assert_eq!(signal, libc::SIGUSR2);
                return;
            }
            thread::yield_now();
        }
        panic!("signal did not reach signalfd");
    }
}
