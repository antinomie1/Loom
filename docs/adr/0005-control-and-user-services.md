# ADR 0005: Control protocol and user managers

- Status: Accepted
- Date: 2026-08-24

## Protocol

System control uses `/run/loom/control.sock`; user control uses
`$XDG_RUNTIME_DIR/loom/control.sock`. Both are Unix `SOCK_SEQPACKET` sockets.
Messages have a small versioned little-endian binary header containing request
ID, operation, status, and bounded payload length. The initial hard packet limit
is 64 KiB; larger results are returned as bounded chunks. Stable status enums
cross the protocol; human strings do not serve as machine error identifiers.

The system socket is mode 0666 so unprivileged users can query status, and uses
`SO_PEERCRED` to restrict mutation to root. Client count, queued output, and
request rate are bounded. A user manager accepts only its own UID and uses a
mode-0600 socket in an owner/mode-validated runtime directory.

The event loop serializes state transitions. Equivalent operations share a
result; conflicting operations execute in receive order. Apply owns the global
configuration transaction while read-only status remains available. Commands
wait for the requested transition or timeout instead of acknowledging queueing.

## Commands

The v1 command surface is:

- `start`, `stop`, `restart`, `reload-service`;
- `status`, `list`, `is-active`, `is-enabled`, `dependencies`;
- `enable`, `disable`, `enable --now`, `disable --now`;
- `reload`, `apply`, `apply --dry-run`, `reset-failed`;
- `timings`, `critical-path`;
- `reboot`, `poweroff`.

Without a name, status summarizes failed services and startup time. Name errors
suggest close matches. Human output goes to stdout/stderr; `--format toml`
provides stable structured output. Exit codes are 0 success, 1 service failure,
2 command/configuration error, 3 permission error, 4 manager unavailable, and 5
timeout.

## User managers

`loom --user` reuses the system manager engine but has no privileged system
interface. `loomctl --user` automatically starts it when its socket is absent:
it takes a startup lock, creates a new session with `setsid`, passes a one-shot
readiness descriptor, and reconnects only after readiness. There is no `/tmp`
fallback when a secure XDG runtime directory is unavailable.

The user manager persists until explicitly stopped or system shutdown. Login
session counting and linger policy are not part of v1. User service installation
does not enable or start a service. User enablement is stored as group membership
in the user's Loom TOML.
