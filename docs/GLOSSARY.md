# Glossary

**Module** — Code with one interface and an implementation. A module may be a
Rust module, crate, or larger slice. Modules should be deep: callers learn a
small interface that hides substantial behavior.

**Interface** — Everything a caller must know: operations, invariants, ordering,
errors, configuration, and performance characteristics.

**Seam** — A location where behavior can be replaced without editing its caller.
A concrete implementation at a seam is an **adapter**. Loom adds seams only for
behavior that actually varies.

**Service definition** — Immutable, validated configuration loaded from one
TOML service file. The filename supplies the service identifier.

**Group definition** — A named virtual node containing dependency relationships.
It has no process. The configured default group roots system startup.

**Configuration snapshot** — A complete set of service and group definitions
that has passed ownership, schema, and graph validation. A manager swaps
snapshots atomically.

**Manager** — The state owner and event loop for either the system instance or
one non-root user instance.

**Desired state** — Whether a service should be active. Manual `start` changes
runtime desired state; enabling changes persistent group membership.

**Observed state** — One of `Inactive`, `Starting`, `Active`, `Stopping`, or
`Failed`, derived from process and readiness events.

**Process attempt** — One generation of a service process. Events carry its
generation so stale exits, notifications, and timers cannot affect a newer
attempt.

**Readiness** — The condition that allows dependants to start: successful exec,
a `READY` notification, or successful completion of a oneshot.

**Enabled service** — A service directly listed by a group. A dependency that is
started indirectly is not enabled.

**Apply** — Validate and swap a configuration snapshot, then reconcile enabled
services and changed active definitions. **Reload** swaps definitions without
changing running processes.

**Linux adapter** — The implementation that owns Linux syscalls and descriptors,
including clone3, pidfd, epoll, signalfd, timerfd, and cgroup v2 operations.

**Sage service specification** — Package-level, init-independent metadata.
It is compiled offline into a Loom service definition and is not read by PID 1.
