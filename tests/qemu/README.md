# PID 1 QEMU smoke test

Build a minimal initramfs containing the release `loom` binary and boot it as
PID 1 with a locally supplied x86_64 Linux kernel:

```sh
tests/qemu/build-initramfs.sh
tests/qemu/boot.sh /path/to/bzImage
```

The guest mounts its API filesystems, loads a strict system snapshot, starts an
oneshot service, and writes `LOOM_BOOT_OK` to the serial console. The test fails
on timeout, an early QEMU exit, a missing marker, or rescue mode.

The kernel is intentionally not downloaded by the test. CI and benchmark runs
must supply the same pinned kernel artifact used by the comparison image.

For diagnosing the comparison side independently, build the minimal systemd
image from an existing rootfs:

```sh
tests/qemu/build-systemd-initramfs.sh /mnt/vm-root
```

The measured same-rootfs pair is produced by `benchmarks/build-images.sh`.

## Correctness regression

```sh
tests/qemu/regression.sh /path/to/bzImage
```

This separate suite runs all Rust test binaries in a minimal guest, checks
complete cleanup of a service with a detached child in a nested cgroup, recovers
from invalid initial configuration and required-service exit/exec failures, and
requires ordered shutdown markers and QEMU exit. Guest logs are retained under
`target/qemu/regression/`. It uses TCG and does not require KVM. CI pins Ubuntu
kernel `6.8.0-138-generic` from the Ubuntu snapshot archive, verifies its package
SHA-256, and retains those logs. It does not build a Sage image or measure
comparative startup performance.
