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

Implementation is in progress. See [the ADR index](docs/adr/README.md) and
[glossary](docs/GLOSSARY.md).

## License

BSD-2-Clause.
