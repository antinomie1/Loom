# ADR 0001: Product scope and priorities

- Status: Accepted
- Date: 2026-08-24

## Decision

Loom is a Linux init and service manager. The system instance can run as PID 1;
the same engine can run one manager per non-root user. `loomctl` is the sole
administrative client. The first release uses a system instance and independent
user instances; there is no additional daemon.

PID 1 owns only:

- early API-filesystem setup required to operate;
- dependency planning and concurrent process launch;
- readiness, process supervision, restart, and failure propagation;
- configuration reload/application and local control;
- rescue, reboot, poweroff, and ordered shutdown.

Logging storage, networking, udev, mounts beyond API filesystems, scheduled jobs,
session tracking, package management, and policy are external programs managed
like other services.

The first release supports `simple` and `oneshot` processes, exec/notify/oneshot
readiness, dependencies, groups, restart policy, system and user services, and a
`systemctl`-like command vocabulary. It does not implement systemd units, D-Bus,
socket/timer/path activation, forking-daemon detection, containers, sandbox
policy, cgroup resource policy, or online PID-1 replacement.

Installing a service definition does not enable or start it. Convenience comes
from defaults, diagnostics, automatic user-manager startup, and combined
commands such as `enable --now`, never from silently changing policy.

## Priority order

1. Correctness and recovery
2. Measured startup performance
3. Small, clear implementation
4. Sage integration
5. Familiar CLI vocabulary
6. Additional kernel adapters

Loom may use Linux-specific facilities whenever they improve correctness or
performance. Portability must not force a lowest-common-denominator Linux path.

## Consequences

A feature outside this scope requires a new ADR and evidence that it belongs in
Loom rather than an external program. The first release is complete only when it
boots and shuts down a Sage image as PID 1, supports user services, integrates
with Sage rebuilds, and passes ADR 0007.
