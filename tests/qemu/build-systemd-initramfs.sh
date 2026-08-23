#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 SYSTEM_ROOT [OUTPUT]" >&2
    exit 2
fi
system_root=${1%/}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
output=${2:-"$repo/target/qemu/systemd-initramfs.cpio.gz"}
case $output in
    /*) ;;
    *) output=$PWD/$output ;;
esac
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT INT TERM

copy_file() {
    source=$1
    [ -f "$system_root$source" ] || {
        echo "comparison root is missing $source" >&2
        exit 1
    }
    mkdir -p "$(dirname -- "$root$source")"
    cp -L "$system_root$source" "$root$source"
}

mkdir -p "$root"/dev "$root"/proc "$root"/sys "$root"/run \
    "$root"/etc "$root"/sbin "$root"/usr/bin \
    "$root"/usr/lib/systemd/system
for file in \
    /usr/lib/systemd/systemd \
    /usr/lib/systemd/systemd-executor \
    /usr/lib/systemd/libsystemd-core-261.so \
    /usr/lib/systemd/libsystemd-shared-261.so \
    /usr/lib/libc.so.6 \
    /usr/lib/libm.so.6 \
    /usr/lib/libmount.so.1 \
    /usr/lib/libblkid.so.1 \
    /usr/lib/libkmod.so.2 \
    /usr/lib/libzstd.so.1 \
    /usr/lib/liblzma.so.5 \
    /usr/lib/libz.so.1 \
    /usr/lib/libcrypto.so.4 \
    /usr/lib/ld-linux-x86-64.so.2 \
    /usr/bin/printf
do
    copy_file "$file"
done
ln -s ../usr/lib/systemd/systemd "$root/sbin/init"
ln -s usr/lib "$root/lib"
ln -s usr/lib "$root/lib64"

cat >"$root/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat >"$root/etc/group" <<'EOF'
root:x:0:
EOF
printf 'uninitialized\n' >"$root/etc/machine-id"
cat >"$root/usr/lib/systemd/system/default.target" <<'EOF'
[Unit]
Description=Benchmark target
Wants=benchmark.service
After=benchmark.service
EOF
cat >"$root/usr/lib/systemd/system/benchmark.service" <<'EOF'
[Unit]
Description=Benchmark readiness marker
DefaultDependencies=no

[Service]
Type=oneshot
ExecStart=/usr/bin/printf BENCHMARK_READY\n
StandardOutput=tty
TTYPath=/dev/console
RemainAfterExit=yes
EOF

mkdir -p "$(dirname -- "$output")"
(
    cd "$root"
    find . -print0 | cpio --null -o --format=newc --owner=0:0 2>/dev/null | gzip -9 >"$output"
)
printf '%s\n' "$output"
