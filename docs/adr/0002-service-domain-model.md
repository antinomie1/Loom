# ADR 0002: Service domain model and state machine

- Status: Accepted
- Date: 2026-08-24

## Definitions

The authoritative terms are in `docs/GLOSSARY.md`. The runtime uses these
separate values:

- immutable `ServiceDefinition` and `GroupDefinition` values;
- one validated immutable `ConfigSnapshot`;
- one `ServiceRuntime` per service;
- one generation-numbered `ProcessAttempt` per launch;
- desired state separate from observed state.

A service identifier matches `[a-zA-Z0-9][a-zA-Z0-9_.@-]{0,127}`. Its filename
is its identifier; the TOML does not repeat it. System and user managers have
separate namespaces. Templates and aliases are not supported in v1.

## States

```text
Inactive -> Starting -> Active -> Stopping -> Inactive
                  \-> Failed <-/
```

A successful oneshot is Active without a PID. Each process attempt receives a
monotonically increasing generation. Exit, readiness, and timeout events for an
old generation are discarded.

A stop during Starting cancels readiness and terminates that attempt. A start
during Stopping waits for complete termination before creating a new attempt.
Restart is always a complete stop followed by a new start.

## Graph

Definitions support four relationships:

- `requires`: readiness and lifetime requirement; failure propagates;
- `wants`: best-effort activation; failure does not propagate;
- `after`: ordering only;
- `conflicts`: both cannot be active.

`before` is not supported. Missing hard dependencies and all ordering/requirement
cycles reject the snapshot with the shortest useful chain. A group is a virtual
node and has no process. A group succeeds after all required members are Active
and every wanted member has either become Active or reached a terminal failure.

A snapshot that directly enables conflicting services is invalid. A manual
start may stop an active conflicting service before starting the requested one.

All graph-ready services are released immediately. Loom has no CPU-count-based
launch throttle; it has a defensive limit of 4096 concurrent Starting services,
which configuration may lower.

## Readiness

- `exec`: EOF on a close-on-exec error pipe proves `execve` succeeded.
- `notify`: the service sends `READY` over its inherited notification socket.
- `oneshot`: exit status zero establishes readiness.

Notify uses `SOCK_SEQPACKET`; `LOOM_NOTIFY_FD` names the inherited descriptor.
Messages are bounded and limited to `READY`, `STATUS <text>`, and `FAIL <text>`.
Closing before `READY` is failure.

## Supervision

Restart policies are `no`, `on-failure`, and `always`. Manual stop never
restarts. Defaults are:

- start/notify timeout: 30 seconds;
- stop timeout: 10 seconds;
- five restarts per five minutes;
- exponential delay from 100 ms to 30 seconds;
- reset after five minutes of stable operation.

A required dependency failure stops its dependants in reverse dependency order.
Wanted dependency failures are reported but do not stop dependants. Stop sends
SIGTERM to the process domain, waits, sends SIGKILL, then waits until the domain
is empty. `--force` starts at SIGKILL.
