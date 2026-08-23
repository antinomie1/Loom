# Boot comparison harness

`boot.py` enforces the sampling and p50/p95 gate in
[ADR 0007](../docs/adr/0007-performance-contract.md). It alternates image order
to reduce thermal/order bias, uses one pinned kernel and QEMU configuration,
retains all raw samples and artifact hashes, and exits unsuccessfully unless
both Loom percentiles are lower.

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
the marker only after the contract's target is ready. The minimal smoke image in
`tests/qemu` is not a valid systemd comparison fixture by itself. Do not publish
a result without retaining the rootfs manifest, kernel configuration, QEMU
version, host state, and raw JSON report.
