#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 SYSTEM_ROOT [OUTPUT_DIRECTORY]" >&2
    exit 2
fi
system_root=${1%/}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${2:-"$repo/target/benchmark"}
case $output in
    /*) ;;
    *) output=$PWD/$output ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
common=$work/common
loader=$system_root/lib64/ld-linux-x86-64.so.2
library_path=$system_root/usr/lib/systemd:$system_root/usr/lib:$system_root/usr/lib64

cargo build --manifest-path "$repo/Cargo.toml" --release --bin loom
mkdir -p "$common"/dev "$common"/proc "$common"/sys "$common"/run \
    "$common"/etc/loom/services "$common"/usr/bin \
    "$common"/usr/lib/loom "$common"/usr/lib/systemd/system

copy_from_system() {
    path=$1
    [ -e "$system_root$path" ] || {
        echo "comparison root is missing $path" >&2
        exit 1
    }
    mkdir -p "$(dirname -- "$common$path")"
    cp -L "$system_root$path" "$common$path"
}

copy_program() {
    path=$1
    copy_from_system "$path"
    "$loader" --list --library-path "$library_path" "$system_root$path" |
        awk -v root="$system_root" '
            $3 ~ ("^" root "/") { print substr($3, length(root) + 1) }
            $1 ~ ("^" root "/") { print substr($1, length(root) + 1) }
        ' | while IFS= read -r dependency; do
            copy_from_system "$dependency"
        done
}

copy_program /usr/lib/systemd/systemd
copy_program /usr/lib/systemd/systemd-executor
copy_program /usr/bin/printf
copy_program /bin/sh
for library in /usr/lib/libmount.so.1 /usr/lib/libkmod.so.2; do
    copy_program "$library"
done
cp "$repo/target/release/loom" "$common/usr/lib/loom/loom"
copy_from_system /usr/lib/libgcc_s.so.1

cat >"$common/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat >"$common/etc/group" <<'EOF'
root:x:0:
EOF
printf 'uninitialized\n' >"$common/etc/machine-id"
cat >"$common/etc/loom/loom.toml" <<'EOF'
schema_version = 1
default_group = "boot"
[groups.boot]
wants = ["benchmark"]
EOF
cat >"$common/etc/loom/services/benchmark.toml" <<'EOF'
schema_version = 1
[process]
command = ["/usr/bin/printf", "BENCHMARK_READY\\n"]
type = "oneshot"
[io]
stdout = "console"
stderr = "console"
EOF
cat >"$common/usr/lib/systemd/system/default.target" <<'EOF'
[Unit]
Description=Benchmark target
Wants=benchmark.service
After=benchmark.service
EOF
cat >"$common/usr/lib/systemd/system/benchmark.service" <<'EOF'
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

mkdir -p "$output"
for system in loom systemd; do
    root=$work/$system
    cp -a "$common" "$root"
    mkdir -p "$root/sbin"
    if [ "$system" = loom ]; then
        ln -s ../usr/lib/loom/loom "$root/sbin/init"
    else
        ln -s ../usr/lib/systemd/systemd "$root/sbin/init"
    fi
    (
        cd "$root"
        find . -print0 | cpio --null -o --format=newc --owner=0:0 2>/dev/null |
            gzip -9 >"$output/$system.cpio.gz"
    )
done
printf '%s\n' "$output/loom.cpio.gz" "$output/systemd.cpio.gz"
