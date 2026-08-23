# Boot comparison harness

`boot.py` enforces the sampling and p50/p95 gate in
[ADR 0007](../docs/adr/0007-performance-contract.md). It alternates image order
to reduce thermal/order bias, uses one pinned kernel and QEMU configuration,
retains all raw samples and artifact hashes, and exits unsuccessfully unless
both Loom percentiles are lower.

Build two initramfs images from one rootfs. Both images contain the same files,
service definitions and readiness command; only `/sbin/init` differs:

```sh
benchmarks/build-images.sh /mnt/vm-root target/benchmark/equivalent
```

Then collect the samples:

```sh
benchmarks/boot.py \
  --kernel /path/to/bzImage \
  --loom-initramfs /path/to/loom.cpio.gz \
  --comparison-initramfs /path/to/systemd.cpio.gz \
  --marker BENCHMARK_READY \
  --runs 30 \
  --output benchmark-results.json
```

The two initramfs artifacts must be built from the same rootfs, contain the same
service graph and readiness probes, use equivalent service semantics, and print
the marker only after the contract's target is ready. `build-images.sh` currently
constructs only the one-service harness; it is useful for exercising measurement
but does not cover all official ADR 0007 scenarios. Do not publish a result
without retaining the rootfs manifest, kernel configuration, QEMU version, host
state, and raw JSON report.
