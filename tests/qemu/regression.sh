#!/bin/sh
# Minimal PID-1 correctness tests; no Sage image or performance measurements.
set -eu
[ "$#" -eq 1 ] || { echo "usage: $0 KERNEL" >&2; exit 2; }
kernel=$1
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
root=$work/root
cargo build --manifest-path "$repo/Cargo.toml" --bins
cargo test --manifest-path "$repo/Cargo.toml" --all-targets --no-run --message-format=json >"$work/artifacts.json"
mkdir -p "$root"/dev "$root"/proc "$root"/sys "$root"/run "$root"/tmp \
    "$root"/etc/loom/services "$root"/usr/bin "$root"/usr/lib/loom "$root"/sbin "$root"/tests
chmod 1777 "$root/tmp"
copy_file() {
    mkdir -p "$(dirname -- "$root$1")"
    cp -L "$1" "$root$1"
}
copy_program() {
    program=$1
    destination=${2:-$1}
    mkdir -p "$(dirname -- "$root$destination")"
    cp -L "$program" "$root$destination"
    ldd "$program" | awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' |
        while IFS= read -r library; do copy_file "$library"; done
}
copy_program "$repo/target/debug/loom" /usr/lib/loom/loom
copy_program "$repo/target/debug/loomctl" /usr/bin/loomctl
# Integration tests embed Cargo's absolute executable paths.
copy_program "$repo/target/debug/loom"
copy_program "$repo/target/debug/loomctl"
ln -s /usr/lib/loom/loom "$root/sbin/init"
for program in /bin/sh /bin/sleep /bin/true /bin/false /bin/kill /usr/bin/touch \
    /usr/bin/printf /usr/bin/grep /usr/bin/cat /usr/bin/mkdir /usr/bin/setsid; do
    copy_program "$program"
done
cp "$root/bin/sh" "$root/bin/real-sh"
python3 - "$work/artifacts.json" >"$work/tests" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    item = json.loads(line)
    if item.get("reason") == "compiler-artifact" and item.get("profile", {}).get("test") and item.get("executable"):
        print(item["executable"])
PY
printf '#!/bin/sh\nset -eu\n' >"$root/tests/run.sh"
while IFS= read -r executable; do
    copy_program "$executable"
    printf '"%s" --test-threads=1\n' "$executable" >>"$root/tests/run.sh"
done <"$work/tests"
cat >"$root/etc/passwd" <<'CONFIG'
root:x:0:0:root:/root:/bin/sh
CONFIG
printf 'root:x:0:\n' >"$root/etc/group"
cat >"$root/driver.sh" <<'DRIVER'
#!/bin/sh
set -eu
trap 'result=$?; if [ "$result" -ne 0 ]; then printf "LOOM_TEST_FAILURE\n"; loomctl poweroff; fi' EXIT
/bin/sh /tests/run.sh
# A setsid child escapes the process group but must remain in its service cgroup.
loomctl start tree
while [ ! -s /run/tree-child ]; do sleep 0.01; done
loomctl stop --force tree
[ ! -d /sys/fs/cgroup/loom/tree-1 ]
loomctl --format toml status
printf 'LOOM_REGRESSION_OK\n'
loomctl poweroff
DRIVER
cat >"$root/tree.sh" <<'TREE'
#!/bin/sh
# Root services may create nested domains; cleanup must remove those too.
mkdir /sys/fs/cgroup/loom/tree-1/nested
cat >/run/detached.sh <<'CHILD'
#!/bin/sh
printf '%s\n' "$$" >/run/tree-child
exec /bin/sleep 30
CHILD
(
    echo 0 >/sys/fs/cgroup/loom/tree-1/nested/cgroup.procs
    exec setsid /bin/sh /run/detached.sh
) &
wait
TREE
cat >"$root/rescue.sh" <<'RESCUE'
#!/bin/real-sh
set -eu
loomctl --format toml status | grep 'mode = "rescue"'
# Reap an adopted orphan while rescue remains available.
/bin/real-sh -c 'sleep 0.05 &'
sleep 0.1
cat >/etc/loom/services/fixed.toml <<'SERVICE'
schema_version = 1
[process]
command = ["/bin/true"]
type = "oneshot"
SERVICE
cat >/etc/loom/loom.toml <<'CONFIG'
schema_version = 1
default_group = "boot"
shutdown_group = "shutdown"
[groups.boot]
requires = ["fixed"]
[groups.shutdown]
wants = ["shutdown-ok"]
CONFIG
loomctl apply
loomctl --format toml status | grep 'mode = "running"'
printf 'LOOM_RESCUE_OK\n'
loomctl poweroff
RESCUE
cat >"$root/etc/loom/services/shutdown-ok.toml" <<'CONFIG'
schema_version = 1
[process]
command = ["/usr/bin/printf", "LOOM_SHUTDOWN_OK\n"]
type = "oneshot"
CONFIG
cat >"$root/etc/loom/services/tree.toml" <<'CONFIG'
schema_version = 1
[process]
command = ["/bin/sh", "/tree.sh"]
CONFIG
cat >"$root/etc/loom/services/driver.toml" <<'CONFIG'
schema_version = 1
[process]
command = ["/bin/sh", "/driver.sh"]
CONFIG
cat >"$root/etc/loom/services/broken.toml" <<'CONFIG'
schema_version = 1
[process]
command = ["/bin/false"]
type = "oneshot"
CONFIG
log_dir="$repo/target/qemu/regression"
mkdir -p "$log_dir"
for scenario in healthy invalid-config required-failure required-exec-failure; do
    cp "$root/bin/real-sh" "$root/bin/sh"
    case $scenario in
        healthy)
            marker=LOOM_REGRESSION_OK
            boot='wants = ["driver"]'
            ;;
        required-failure | required-exec-failure)
            marker=LOOM_RESCUE_OK
            boot='requires = ["broken"]'
            ;;
        invalid-config)
            marker=LOOM_RESCUE_OK
            boot='invalid configuration'
            cat >"$root/bin/sh" <<'SHELL'
#!/bin/real-sh
if [ "$#" -eq 0 ]; then exec /bin/real-sh /rescue.sh; fi
exec /bin/real-sh "$@"
SHELL
            chmod 755 "$root/bin/sh"
            ;;
    esac
    if [ "$scenario" = required-exec-failure ]; then
        cat >"$root/etc/loom/services/broken.toml" <<'CONFIG'
schema_version = 1
[process]
command = ["/missing-executable"]
CONFIG
    fi
    cat >"$root/etc/loom/loom.toml" <<CONFIG
schema_version = 1
default_group = "boot"
shutdown_group = "shutdown"
rescue_command = ["/bin/sh", "/rescue.sh"]
[groups.boot]
$boot
[groups.shutdown]
wants = ["shutdown-ok"]
CONFIG
    (cd "$root" && find . -print0 | cpio --null -o --format=newc --owner=0:0 2>/dev/null | gzip -1) >"$work/root.cpio.gz"
    log=$log_dir/$scenario.log
    if ! timeout 90 qemu-system-x86_64 -machine accel=tcg -cpu max -m 512M \
        -nodefaults -display none -serial stdio -no-reboot \
        -kernel "$kernel" -initrd "$work/root.cpio.gz" \
        -append 'console=ttyS0 panic=-1 rdinit=/sbin/init' >"$log" 2>&1; then
        cat "$log"; echo "$scenario: guest did not shut down cleanly" >&2; exit 1
    fi
    if ! grep -q "^$marker" "$log" || ! grep -q '^LOOM_SHUTDOWN_OK' "$log"; then
        cat "$log"; echo "$scenario: missing completion markers" >&2; exit 1
    fi
    printf '%s: passed\n' "$scenario"
done
