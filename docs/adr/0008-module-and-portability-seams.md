# ADR 0008: Module and portability seams

- Status: Accepted
- Date: 2026-08-24

## Structure

Start with one Cargo package containing a library and two binaries, not a crate
per noun. Logical modules are:

- model/config: strict TOML values, layering, identity resolution, graph checks;
- runtime: state machine, planning, scheduling, and failure propagation;
- protocol: bounded control messages and stable errors;
- Linux adapter: descriptors, process creation, epoll, signals, timers, cgroups;
- `loom`: system/user manager and offline compile/validate entry points;
- `loomctl`: CLI parsing, presentation, and atomic configuration edits.

The runtime interface consumes typed events and produces typed effects. The
Linux manager executes effects and feeds resulting events back. Tests cross the
same interface. This keeps process policy independent of syscalls without a
public, speculative `Platform` trait. A second real kernel adapter may justify
extracting that seam later.

Definitions are immutable values. Descriptor ownership, event registration, and
attempt generations remain local to the modules that enforce them. Callers do
not reproduce validation, state transitions, error mapping, or Sage conversion.
A module is split only when that improves its interface or separates a second
responsibility, not to satisfy file-size aesthetics.

## Portability

The supported v1 adapter is Linux x86_64. Architecture-neutral code contains no
Linux types. The Linux adapter may use clone3, pidfd, epoll, signalfd, timerfd,
openat2, and cgroup v2 directly. Future L9 or other-kernel work implements an
adapter for the established event/effect model; it does not remove fast Linux
paths or require Loom to compile for L9 today.

## Dependencies and Rust

Initial runtime dependencies are limited to `serde`, `toml_edit`, `rustix`,
`libc` where rustix lacks an operation, `lexopt`, and `thiserror`. Do not add an
async runtime or duplicate rustix with nix. Property/fuzz/benchmark tooling may
be dev-only dependencies.

Use Rust 2024 and the pinned Sage Rust toolchain. Model and runtime forbid unsafe
code. The Linux adapter documents every unsafe block's preconditions, ownership,
and lifetime. There is no hard source-line limit; interface depth, duplication,
release size, RSS, and measured latency are the constraints.

## Consequences

The initial repository contains no empty portability crate, generic adapter
hierarchy, or pass-through module. New dependencies and seams require an actual
variation, deleted duplicate code, or measured correctness/performance value.
