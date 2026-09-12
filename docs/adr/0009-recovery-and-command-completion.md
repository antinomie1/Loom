# ADR 0009: Recovery and command completion

- Status: Accepted
- Date: 2026-09-12
- Refines: ADR 0002, ADR 0003, ADR 0004, ADR 0005

## Process completion

A process attempt ends only after its leader exits and its complete process
domain is empty. Cgroup v2 uses `cgroup.events` notifications and the recursive
`populated` value. Empty nested cgroups are removed before acknowledging stop.
Process-group fallback is explicitly weaker for descendants that create new
sessions. Managers become child subreapers and reap adopted children without
consuming the exit status of tracked leaders or helpers.

Stop and reload helpers use the same event loop, pidfd tracking and process-domain
cleanup as services. They never synchronously wait in the manager loop. A stop
helper completes before SIGTERM; the stop deadline bounds the whole operation,
and SIGKILL also cancels the helper. Zero disables a helper's timeout. No new
attempt starts while the previous attempt's helper or descendants remain.

## Recovery

Invalid initial system configuration creates an empty in-memory rescue snapshot.
The control socket, child reaping, timers and shutdown remain available. Failure
of a required boot chain also enters rescue, including pre-exec failure and a
required simple service that exits without a restartable attempt. Required boot
chains remain monitored after initial activation; wanted service failures alone
do not enter rescue. The failure chain is exposed by status.

`rescue_command` is an absolute argv array in the manager document and defaults
to `["/bin/sh"]`. Its console session is supervised and retried no more than once
per second. A successful `apply` resumes normal management. An existing rescue
shell may finish its current command and exit; it is not restarted after
recovery. Shutdown terminates and drains that session. Failures before the
control/event-loop facilities can be established retain the last-resort console
shell in `loom`.

## Control surface

- `enable --now` and `disable --now` await the requested runtime transition and
  report failures or the existing bounded control-request timeout.
- `stop --force` preserves dependency ordering and starts each stop at SIGKILL.
- `apply --dry-run` loads and validates the prospective snapshot and account
  records, then uses the same affected-service calculation as apply. It neither
  replaces the snapshot nor changes configuration files or processes. If no user
  manager is running, it reports manager unavailable without auto-starting one.
- Plans list `start`, `stop`, `restart`, `update`, and `remove`. Description and
  restart-policy changes update definitions without restarting an active service.
- `--format toml` produces a versioned report with the numeric protocol status
  and typed service/dependency/timing/plan data. Optional fields are omitted when
  unavailable. Timing fields use the manager's monotonic millisecond clock.
- Responses are split at the existing packet limit, with bounded nonblocking
  output queues. The client preserves UTF-8 bytes across chunk boundaries.
- Requests received during shutdown or a pending apply cannot mutate state.
  Command errors return a protocol failure rather than tearing down the manager.

The v1 binary header remains unchanged. Additive operation IDs are 19 for dry-run
apply and 20 for forced stop. The reserved `toml\n` payload prefix requests TOML
output; service identifiers cannot contain a newline.

## User-manager startup

The installed executable is located at `<prefix>/lib/loom/loom` relative to
`<prefix>/bin/loomctl`; adjacent development binaries remain supported. The
launcher validates XDG runtime ownership and permissions before creating its
startup lock. Concurrent launchers share that lock. An inherited one-shot pipe
acknowledges completed manager initialization; failed or timed-out startups are
terminated instead of leaving an untracked manager behind.

## Verification boundary

Unit and integration tests cover helper completion, timeouts, forced stops,
process-group cleanup, dry-run parity, structured response chunking and concurrent
installed-layout startup. The minimal QEMU regression runs those tests as root,
checks nested cgroup cleanup, exercises invalid-configuration and required-chain
rescue, and verifies ordered poweroff. CI also runs the host tests as a non-root
user. This is correctness verification, not a complete Sage image or an ADR 0007
performance acceptance run. Those release gates remain outstanding.
