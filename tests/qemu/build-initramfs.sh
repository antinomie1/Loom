#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
output=${1:-"$repo/target/qemu/loom-initramfs.cpio.gz"}
case $output in
    /*) ;;
    *) output=$PWD/$output ;;
esac
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT INT TERM

cargo build --manifest-path "$repo/Cargo.toml" --release --bin loom

copy_file() {
    source=$1
    destination="$root$source"
    mkdir -p "$(dirname -- "$destination")"
    cp -L "$source" "$destination"
}

copy_program() {
    program=$1
    destination=${2:-$program}
    mkdir -p "$(dirname -- "$root$destination")"
    cp -L "$program" "$root$destination"
    ldd "$program" | awk '
        /=> \// { print $3 }
        /^[[:space:]]*\// { print $1 }
    ' | while IFS= read -r library; do
        copy_file "$library"
    done
}

mkdir -p "$root"/dev "$root"/proc "$root"/sys "$root"/run \
    "$root"/etc/loom/services "$root"/usr/bin "$root"/sbin
copy_program "$repo/target/release/loom" /sbin/init
copy_program /bin/sh
copy_program /usr/bin/printf
copy_file /etc/ld.so.cache

cat >"$root/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat >"$root/etc/group" <<'EOF'
root:x:0:
EOF
cat >"$root/etc/loom/loom.toml" <<'EOF'
schema_version = 1
default_group = "boot"

[groups.boot]
wants = ["boot-ok"]
EOF
cat >"$root/etc/loom/services/boot-ok.toml" <<'EOF'
schema_version = 1
[process]
command = ["/usr/bin/printf", "LOOM_BOOT_OK\\n"]
type = "oneshot"
[io]
stdout = "console"
stderr = "console"
EOF

mkdir -p "$(dirname -- "$output")"
(
    cd "$root"
    find . -print0 | cpio --null -o --format=newc --owner=0:0 2>/dev/null | gzip -9 >"$output"
)
printf '%s\n' "$output"
