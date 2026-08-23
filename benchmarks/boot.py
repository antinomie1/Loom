#!/usr/bin/env python3
"""Run the ADR 0007 QEMU comparison and retain every raw sample."""

import argparse
import hashlib
import json
import math
import os
import selectors
import subprocess
import time
from pathlib import Path


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def percentile(samples: list[float], fraction: float) -> float:
    ordered = sorted(samples)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def run_once(kernel: Path, initramfs: Path, marker: bytes, timeout: float) -> float:
    command = [
        "qemu-system-x86_64",
        "-machine", "accel=kvm:tcg",
        "-m", "128M",
        "-nodefaults",
        "-display", "none",
        "-serial", "stdio",
        "-no-reboot",
        "-kernel", str(kernel),
        "-initrd", str(initramfs),
        "-append", "console=ttyS0 panic=-1 rdinit=/sbin/init",
    ]
    started = time.monotonic_ns()
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    assert process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output = bytearray()
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            if process.poll() is not None:
                break
            for key, _ in selector.select(0.1):
                block = os.read(key.fd, 4096)
                if not block:
                    continue
                output.extend(block)
                if marker in output:
                    return (time.monotonic_ns() - started) / 1_000_000
        raise RuntimeError(
            f"marker {marker!r} not observed for {initramfs}; guest output:\n"
            + output.decode(errors="replace")
        )
    finally:
        process.terminate()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--loom-initramfs", required=True, type=Path)
    parser.add_argument("--comparison-initramfs", required=True, type=Path)
    parser.add_argument("--marker", default="BENCHMARK_READY")
    parser.add_argument("--runs", type=int, default=30)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--output", type=Path, default=Path("benchmark-results.json"))
    args = parser.parse_args()
    if args.runs < 30:
        parser.error("ADR 0007 requires at least 30 runs")
    for path in (args.kernel, args.loom_initramfs, args.comparison_initramfs):
        if not path.is_file():
            parser.error(f"artifact does not exist: {path}")

    samples: dict[str, list[float]] = {"loom": [], "comparison": []}
    marker = args.marker.encode()
    for iteration in range(args.runs):
        order = ("loom", "comparison") if iteration % 2 == 0 else ("comparison", "loom")
        for system in order:
            initramfs = (
                args.loom_initramfs if system == "loom" else args.comparison_initramfs
            )
            elapsed = run_once(args.kernel, initramfs, marker, args.timeout)
            samples[system].append(elapsed)
            print(f"{iteration + 1:02d}\t{system}\t{elapsed:.3f} ms", flush=True)

    summary = {
        system: {
            "p50_ms": percentile(values, 0.50),
            "p95_ms": percentile(values, 0.95),
            "samples_ms": values,
        }
        for system, values in samples.items()
    }
    qemu_version = subprocess.check_output(
        ["qemu-system-x86_64", "--version"], text=True
    ).splitlines()[0]
    report = {
        "contract": "docs/adr/0007-performance-contract.md",
        "qemu_version": qemu_version,
        "kernel_sha256": digest(args.kernel),
        "loom_initramfs_sha256": digest(args.loom_initramfs),
        "comparison_initramfs_sha256": digest(args.comparison_initramfs),
        "marker": args.marker,
        "runs": args.runs,
        "summary": summary,
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    loom = summary["loom"]
    comparison = summary["comparison"]
    passed = (
        loom["p50_ms"] < comparison["p50_ms"]
        and loom["p95_ms"] < comparison["p95_ms"]
    )
    print(json.dumps(summary, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
