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
