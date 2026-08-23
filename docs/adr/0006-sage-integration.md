# ADR 0006: Sage, channels, Recipes, and init switching

- Status: Accepted
- Date: 2026-08-24

## Ownership

Sage owns packages, channel resolution, declarative provider selection, and
rebuild transactions. Loom owns the active service graph and processes. PID 1
never reads Sage LMDB or package state.

A package carries an init-independent Sage service specification. Sage resolves
runtime channels into explicit executable paths and environment values. Loom's
offline `compile-service --from-sage` command is the single adapter from that
specification to a native Loom v1 service. `loom validate --root DIR` validates
a staged target. Sage orchestrates these tools rather than duplicating Loom's
mapping in C++.

## Sage service schema

New Recipes use Sage `service.toml` schema v2 with argv arrays and a required,
non-empty `architectures` array. Canonical values are `amd64`, `aarch64`, and
`any`; `any` cannot be combined with a concrete architecture. Sage continues to
read v1 strings through a strict word tokenizer but never executes them as a
shell; v1 defaults to `architectures = ["any"]` and all existing Recipes migrate
to v2.

Installing a package with a service only installs its definition. It does not
enable or start it. Upgrading an already-enabled service regenerates definitions
and applies execution-affecting changes. Removing an enabled service first stops
it and removes its group membership. Offline `--root` operations never contact
the host manager; dry-runs perform no persistent writes or process control.

## Rebuild

For Loom, `sage rebuild`:

1. reads every installed Sage service specification;
2. generates a complete temporary `/usr/lib/loom/services` tree;
3. validates the staged root;
4. atomically replaces the generated tree;
5. calls `loomctl apply` only for the current root under a running Loom manager.

Generation failure preserves the previous valid tree. Administrator-owned
`/etc/loom` is not regenerated or deleted.

Switching `virtual/init` installs the target provider, validates its complete
service graph, atomically changes `/sbin/init`, updates provider state, and runs
the configured initramfs trigger. PID 1 is never replaced online; the change
takes effect at reboot. The old provider remains recoverable until the guarded
rebuild succeeds.

## Packaging

The Loom package uses BSD-2-Clause and provides `loom` and `virtual/init`:

```text
/usr/lib/loom/loom
/usr/bin/loomctl
/sbin/init -> /usr/lib/loom/loom
/usr/lib/loom/services/
/etc/loom/loom.toml
```

The administrator configuration is a conffile. Explicitly selecting Loom as the
init provider installs a minimal boot group for local mounts, udev and trigger,
D-Bus, networking, DNS, time synchronization, getty, and shutdown helpers.
This explicit provider choice is not permission for later daemon packages to
auto-enable themselves.

Recipes must build Loom reproducibly. Because Recipes currently has no Rust
compiler package, add a pinned official `rust-bin` package that supplies rustc,
Cargo, and `toolchain/rust`; never use undeclared host `/opt` tools. Rust source
self-hosting is outside the Loom v1 scope.
