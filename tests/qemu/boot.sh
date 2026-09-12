#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 KERNEL [INITRAMFS]" >&2
    exit 2
fi
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
kernel=$1
initramfs=${2:-"$repo/target/qemu/loom-initramfs.cpio.gz"}
[ -f "$kernel" ] || { echo "kernel not found: $kernel" >&2; exit 2; }
if [ ! -f "$initramfs" ]; then
    "$repo/tests/qemu/build-initramfs.sh" "$initramfs" >/dev/null
fi

log=$(mktemp)
trap 'rm -f "$log"' EXIT INT TERM
qemu-system-x86_64 \
    -machine accel=tcg \
    -cpu max \
    -m 128M \
    -nodefaults \
    -display none \
    -serial stdio \
    -no-reboot \
    -kernel "$kernel" \
    -initrd "$initramfs" \
    -append "console=ttyS0 panic=-1 rdinit=/sbin/init" \
    >"$log" 2>&1 &
qemu_pid=$!

passed=false
for _ in $(seq 1 100); do
    if grep -q '^LOOM_BOOT_OK' "$log"; then
        passed=true
        break
    fi
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
kill "$qemu_pid" 2>/dev/null || true
wait "$qemu_pid" 2>/dev/null || true
cat "$log"

if [ "$passed" != true ]; then
    echo "Loom QEMU boot marker was not observed" >&2
    exit 1
fi
if grep -q 'entering rescue mode' "$log"; then
    echo "Loom entered rescue mode during QEMU boot" >&2
    exit 1
fi
