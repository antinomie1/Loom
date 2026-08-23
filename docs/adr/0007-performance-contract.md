# ADR 0007: Performance and release contract

- Status: Accepted
- Date: 2026-08-24

## Goal

High performance is a permanent product constraint. The Linux boot path must be
faster than systemd for an equivalent supported service graph. This is a release
gate, not an unbounded claim about all machines or configurations.

## Measurement

Boot time starts when the kernel enters the first userspace instruction and ends
when the configured default group satisfies ADR 0002 readiness. Loom and systemd
runs use the same machine or VM, kernel, root filesystem, service executables,
dependency graph, readiness conditions, and cold/warm-cache policy.

Official evidence includes:

- at least 30 cold boots of a reproducible QEMU Sage image;
- at least 30 boots on the reference i5-13600KF machine;
- synthetic graphs containing 10, 100, and 1000 short services;
- the real Sage default boot graph;
- comparison with both the pinned Recipes systemd and the current host baseline.

Reports publish scripts, raw samples, versions, p50, p95, critical path, PID-1 CPU
time, process count, and peak RSS. A scenario passes only when Loom's p50 and p95
are both lower than systemd beyond measured uncertainty. A release fails if any
official supported scenario regresses or if the real boot graph does not win.
Claims are limited to the published scenarios.

## Development discipline

Microbenchmarks cover TOML parsing, graph validation, scheduling decisions, and
protocol encoding. Integration benchmarks cover process creation and readiness.
Every performance change includes before/after evidence and preserves all
correctness tests. No binary cache, custom parser, allocator, lock-free data
structure, or polling loop is added before profiling identifies its cost.

The normal runtime records only four fixed timestamps per attempt. Benchmark
builds may add detailed events. Performance-sensitive commits update stored
baselines; unrelated commits must not produce a statistically meaningful
regression.

## Build profile

The first release dynamically links the Sage distribution libc. Release builds
use optimization level 3, one codegen unit, stripping with separate debug data,
and aborting panics. LTO mode and target CPU baseline are chosen by benchmark
while remaining compatible with the Sage distribution. Static linking is added
only if measured startup benefit justifies its compatibility cost.
