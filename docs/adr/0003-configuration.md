# ADR 0003: TOML configuration and transactional application

- Status: Accepted
- Date: 2026-08-24

## Service format

Loom configuration is TOML. Every document declares `schema_version = 1`.
Unknown fields, unsupported versions, duplicate identifiers, invalid identities,
unsafe ownership/modes, missing hard dependencies, and graph cycles reject the
entire snapshot.

A native service has this shape; omitted optional fields use the shown defaults:

```toml
schema_version = 1
description = "Example"

[process]
command = ["/usr/bin/example", "--foreground"]
type = "simple"                 # simple | oneshot
readiness = "exec"              # exec | notify
user = "root"
group = "root"
working_directory = "/"
environment = {}
umask = 0o022

[dependencies]
requires = []
wants = []
after = []
conflicts = []

[supervision]
restart = "no"                  # no | on-failure | always
start_timeout_ms = 30000
stop_timeout_ms = 10000
restart_limit = 5
restart_window_ms = 300000
restart_reset_ms = 300000
restart_backoff_max_ms = 30000

[io]
stdout = "console"
stderr = "console"
```

Commands are argv arrays with absolute executable paths. Loom performs no shell
or environment expansion. Shell use must be explicit as `/bin/sh -c`. Optional
`[actions]` stop/reload commands are also argv arrays. A successful stop helper
does not replace verification that the process domain is empty. IO may select
console, null, append-only file, or an external Unix logger socket; Loom does not
rotate or index logs.

System identities may be names or numbers. Snapshot loading resolves names from
local `/etc/passwd` and `/etc/group`, including supplementary groups, and stores
numbers. PID 1 does not invoke network NSS. The shown root identity is the system
service default; a user service defaults to its manager's UID and primary GID.
A timeout value of zero explicitly disables that timeout.

## Layering

Service files are whole-file overrides, in ascending priority:

1. `/usr/lib/loom/services/`
2. `/etc/loom/services/`
3. `/run/loom/services/`

User instances use:

1. `/usr/lib/loom/user/services/`
2. `$XDG_CONFIG_HOME/loom/services/`
3. `$XDG_RUNTIME_DIR/loom/services/`

There are no drop-ins or field merges. System files must be root-owned and not
writable by group/other; user files must be owned by that user. Symbolic links
and paths escaping a configuration root are rejected. Loading uses directory-
relative descriptors and `openat2` restrictions where available.

## Groups and enablement

`/etc/loom/loom.toml` declares `default_group` and named groups. User instances
use `$XDG_CONFIG_HOME/loom/loom.toml`.

```toml
schema_version = 1
default_group = "boot"

[groups.boot]
wants = ["dbus", "network"]
```

A service is enabled only when directly listed by a group. `enable` edits group
membership; `start` changes only runtime desired state. Commands preserve TOML
comments and use lock, temporary file, fsync, and atomic rename.

## Snapshot operations

`reload` parses, validates, and atomically swaps definitions without changing
running processes. Existing runtimes retain the definition for their current
attempt. `apply` first performs reload, then:

- stops removed or disabled services;
- starts newly enabled services;
- restarts active services whose process, readiness, identity, working directory,
  environment, actions, or IO changed;
- reconciles graph changes without restarting for description-only or restart-
  policy-only changes.

`apply --dry-run` returns the plan without writes or process control. Snapshot
replacement is atomic; external process side effects are not transactionally
reversible and failures are reported as reconciliation failures.
