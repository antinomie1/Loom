# Loom contributor rules

## Product invariants

- Correctness and recovery come before speed; speed comes before compatibility.
- A release must beat the comparison systemd graph under the benchmark contract
  in ADR 0007. Do not claim results beyond measured scenarios.
- PID 1 owns process supervision, dependency scheduling, shutdown, and its
  control interface. Do not add logging, networking, device management, package
  management, or session management to it.
- Installing a service definition never enables or starts it.
- System services never execute an implicit shell.
- Linux optimizations are not weakened for hypothetical portability.

## Design rules

- Use the terms in `docs/GLOSSARY.md` consistently.
- Prefer deep modules: small interfaces hiding complete behavior. Do not add
  pass-through wrappers, speculative traits, or one-file-per-type structure.
- Keep service definitions immutable. Keep desired state, observed state, and
  process attempts separate.
- Keep the model and runtime safe Rust. Confine required `unsafe` to the Linux
  adapter, document each safety invariant, and wrap every owned descriptor.
- Use mature lightweight crates when they remove meaningful code or improve
  correctness. Do not add an async runtime, duplicate syscall crates, or an
  optimization without evidence.
- Reject unknown configuration fields and unsupported schema versions.
- Preserve configuration comments when a Loom command edits user-owned TOML.

## Required checks

Once the Rust workspace exists, run before every code commit:

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Run affected PID-1/QEMU, Sage, Recipes, and benchmark suites for changes at
those seams. Never replace a correctness test with a benchmark.

## Documentation

- Architectural decisions belong in `docs/adr/`. Amend an unimplemented ADR;
  supersede an implemented decision with a new ADR.
- Update `docs/GLOSSARY.md` when adding a domain term.
- Keep README and CLI documentation factual and current; omit aspirations that
  lack an acceptance test.
- Keep this file short. It contains rules, not an architecture duplicate.

## Commits

- Commit one coherent, tested feature at a time. Documentation establishing the
  contract precedes implementation.
- Do not commit empty scaffolding, unrelated cleanup, generated build output, or
  broken intermediate states.
- Cross-repository work is committed in dependency order: Loom, Sage, Recipes.
  Use matching feature identifiers where practical; each repository must pass
  independently.
- Use imperative subjects with a conventional prefix such as `docs:`, `feat:`,
  `fix:`, `test:`, or `perf:`.
