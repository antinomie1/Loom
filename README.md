# Loom

Loom is a small, high-performance Linux init and service manager written in
Rust. It provides a `systemctl`-like command vocabulary without implementing
the systemd unit or D-Bus interfaces.

## Goals

- Boot a Sage-based Linux system faster than an equivalent systemd service graph.
- Run as PID 1 and supervise system and non-root user services.
- Start every dependency-ready service concurrently.
- Keep configuration declarative, strict, and TOML-only.
- Integrate with Sage rebuilds and channel-derived environments without making
  PID 1 depend on Sage or LMDB.
- Use Linux-specific performance and correctness features while keeping the
  runtime model portable to a future kernel adapter.

## Non-goals for the first release

- systemd unit or D-Bus compatibility
- journaling, networking, device management, or scheduled-job subsystems
- socket, timer, or path activation
- containers, seccomp policy, namespace setup, or cgroup resource policy
- online replacement of PID 1

## Status

Implementation is in progress. Native TOML validation, dependency scheduling,
pidfd/cgroup supervision, the epoll manager, user instances, the local control
protocol, PID-1 API mounts/rescue, and Sage service compilation are implemented.
Recovery keeps the control interface available; helpers run asynchronously and
stop waits for the complete process domain. Dry-run plans, structured TOML
reports, forced stop and installed-layout user-manager startup are supported.
A minimal PID-1 QEMU regression suite covers recovery, process cleanup and
ordered shutdown. The complete Sage image fixture and comparative performance
acceptance results remain outstanding.

```sh
cargo build --release
cargo test --all-targets
# With a pinned local kernel:
tests/qemu/boot.sh /path/to/bzImage
tests/qemu/regression.sh /path/to/bzImage

# User manager is started automatically when needed.
loomctl --user status
loomctl --user start example
loomctl --user timings
loomctl --user critical-path
loomctl --user apply --dry-run --format toml
loomctl --user enable --now example
```

See [the ADR index](docs/adr/README.md) and [glossary](docs/GLOSSARY.md).

## License

BSD-2-Clause.
