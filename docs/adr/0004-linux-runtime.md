# ADR 0004: Linux process supervision and PID 1 behavior

- Status: Accepted
- Date: 2026-08-24

## Platform baseline

The first supported platform is x86_64 Linux 5.10 or newer. The system manager
requires pidfd support. The preferred process path is
`clone3(CLONE_PIDFD | CLONE_INTO_CGROUP)`, with a race-safe fallback to process
creation, `pidfd_open`, and cgroup attachment when individual clone3 features
are unavailable.

One epoll loop owns pidfds, the control listener, readiness sockets, signalfd,
and timerfds. Processes run concurrently; Loom does not allocate one thread per
service or depend on an async runtime. Bounded workers are permitted only for
measured blocking/CPU preparation that cannot remain outside the event loop.

Each service receives a cgroup v2 process domain. PID 1 establishes Loom's
cgroup subtree when permitted. A restricted container or user manager without
delegation falls back to a process group and reports the weaker guarantee.

Child setup occurs in a fixed order: descriptor setup, session/process group,
supplementary groups, GID, UID, working directory, umask, environment, then
exec. An error pipe reports pre-exec failures. Owned file descriptors use RAII.
Required unsafe code is confined to the Linux adapter with written safety
invariants.

## Environment

System services receive a deterministic base PATH plus HOME, USER, LOGNAME, and
SHELL from their resolved local account. They do not inherit PID 1's environment.
A user manager captures a sanitized startup environment; definitions and
Sage-generated channel bindings override it explicitly.

## PID 1

Before loading services, PID 1 ensures the API filesystems needed for operation:
`/proc`, `/sys`, `/run`, and cgroup v2. Device discovery, fstab mounts, swap,
networking, and policy are external services.

Signals have fixed meanings:

- SIGCHLD: reap all children and adopted orphans;
- SIGTERM: ordered poweroff;
- SIGINT: ordered reboot;
- SIGHUP: reload without apply;
- SIGUSR1: reconnect an external logger;
- unsafe or irrelevant signals: ignore or diagnose.

A broken initial snapshot or failed required boot chain enters Rescue. PID 1
continues reaping and serving control requests, reports the failure chain on the
console, and rate-limits a configurable rescue command whose default is
`/bin/sh`. A repaired configuration can be applied without reboot.

Shutdown rejects new mutating requests, stops services in reverse dependency
order, runs the shutdown group, kills remaining Loom process domains, syncs
filesystems, and calls the reboot syscall. Unmount, swap, and encrypted-volume
policy remain ordinary shutdown services.

## Logging and timing

Loom is not a log store. Services default to console and may select null,
append-only files, or an external logger socket. The manager retains only a
bounded in-memory ring of short state/error events.

Each process attempt stores fixed monotonic timestamps for queued, spawned,
ready, and exited. Detailed tracing is opt-in and not part of the normal hot
path.
