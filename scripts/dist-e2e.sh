#!/usr/bin/env bash
# shellcheck disable=SC2015 # `a && b || fail …`: fail exits
# Release packages end to end on this machine (ADR-0009; evidence for the distribution spec).
# Packs two locally built release binaries with scripts/package.sh, serves them with
# `python3 -m http.server` on a free loopback port as GitHub Releases + the crates.io index, then
# checks install.sh (success, tampered package, setup only on a terminal), the binary self-upgrade
# (bad checksum, success) and install-method detection. Everything lives in one temp directory
# with its own HOME / config dir / TMPDIR; the server is stopped on exit.
#   scripts/dist-e2e.sh <old-uniflo> <new-uniflo>     # <new> must report a newer version
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
[[ $# -eq 2 ]] || { echo "usage: scripts/dist-e2e.sh <old-uniflo> <new-uniflo>" >&2; exit 2; }
old_bin=$1 new_bin=$2
old_v=$("$old_bin" --version | awk '{print $2}')
new_v=$("$new_bin" --version | awk '{print $2}')
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64) target=aarch64-unknown-linux-musl ;;
  *) echo "no release target for this machine" >&2; exit 2 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/uniflo-dist-e2e.XXXXXX")
server_pid=
cleanup() {
  if [[ -n $server_pid ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*" >&2; exit 1; }
sha() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
field() { python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))' "$1" "$2"; }

srv=$work/srv
publish() {
  local v=$1 bin=$2 d="$srv/releases/download/v$1" pkg
  mkdir -p "$d"
  pkg=$("$root/scripts/package.sh" "$v" "$target" "$bin" "$d")
  (cd "$d" && shasum -a 256 "$(basename "$pkg")" >SHA256SUMS)
  if [[ ${3:-} == latest ]]; then
    mkdir -p "$srv/releases/latest/download"
    cp "$d/SHA256SUMS" "$srv/releases/latest/download/"
  fi
}
publish "$old_v" "$old_bin" latest
publish "$new_v" "$new_bin"
mkdir -p "$srv/index"
printf '{"name":"uniflo","vers":"%s","yanked":false}\n' "$old_v" "$new_v" >"$srv/index/uniflo"
pkg_old=$srv/releases/download/v$old_v/uniflo-$old_v-$target.tar.gz
sums_new=$srv/releases/download/v$new_v/SHA256SUMS

port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -I -m http.server "$port" --bind 127.0.0.1 --directory "$srv" >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 50); do curl -fsS "http://127.0.0.1:$port/index/uniflo" >/dev/null 2>&1 && break; sleep 0.1; done
echo "release server: http://127.0.0.1:$port (pid $server_pid) · $target · $old_v → $new_v"

# A clean environment per machine: only system tools on PATH, everything Uniflo writes under $m.
in_machine() {
  local m=$1
  shift
  mkdir -p "$m/home" "$m/tmp"
  env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$m/home" UNIFLO_HOME="$m/home" UNIFLO_CONFIG_DIR="$m/cfg" \
    TMPDIR="$m/tmp" UNIFLO_INSTALL_DIR="$m/bin" UNIFLO_RELEASE_BASE_URL="http://127.0.0.1:$port/releases" \
    UNIFLO_UPDATE_INDEX_URL="http://127.0.0.1:$port/index/uniflo" "$@"
}

# --- install.sh, not on a terminal ---------------------------------------------------------
m=$work/a
in_machine "$m" sh "$root/scripts/install.sh" </dev/null >"$work/a.out" 2>&1 || fail "install.sh exit $?: $(cat "$work/a.out")"
[[ "$("$m/bin/uniflo" --version)" == "uniflo $old_v" ]] || fail "installed version"
[[ "$(sha "$m/bin/uniflo")" == "$(sha "$old_bin")" ]] || fail "installed bytes differ from the release binary"
[[ "$(field "$m/cfg/install.json" method)" == binary && "$(field "$m/cfg/install.json" version)" == "$old_v" &&
  "$(field "$m/cfg/install.json" target)" == "$target" && "$(field "$m/cfg/install.json" path)" == "$(cd "$m/bin" && pwd -P)/uniflo" ]] ||
  fail "install.json: $(cat "$m/cfg/install.json")"
grep -q '跳过 uniflo setup' "$work/a.out" && [[ ! -e $m/cfg/setup.json ]] || fail "setup ran off a terminal"
[[ -z "$(ls -A "$m/tmp")" ]] || fail "install.sh left temp files"
pass "install.sh (non-TTY): uniflo $old_v installed, install.json written, setup skipped, temp dir cleaned"
sed 's/^/      /' "$work/a.out"

# --- install.sh with one byte of the package flipped ---------------------------------------
cp "$pkg_old" "$work/pkg.bak"
python3 -I -c 'import sys; p=sys.argv[1]; b=bytearray(open(p,"rb").read()); b[len(b)//2]^=1; open(p,"wb").write(b)' "$pkg_old"
m=$work/b
code=0
in_machine "$m" sh "$root/scripts/install.sh" </dev/null >"$work/b.out" 2>&1 || code=$?
[[ $code -ne 0 ]] || fail "tampered package installed"
[[ ! -e $m/bin && ! -e $m/cfg/install.json && -z "$(ls -A "$m/tmp")" ]] || fail "files left after a checksum mismatch"
pass "install.sh (tampered package): exit $code, no install dir, no install.json, temp dir cleaned"
sed 's/^/      /' "$work/b.out"
cp "$work/pkg.bak" "$pkg_old"

# --- install.sh on a terminal: setup runs unless --no-setup / UNIFLO_NO_SETUP=1 ---------------
# `script` gives the installer a pty; "4" answers setup's first question with "skip".
tty_install() {
  local m=$1
  shift
  printf '4\n' | in_machine "$m" "$@" >"$m.out" 2>&1
}
installer=(perl -e 'alarm 60; exec @ARGV or die' script -q /dev/null sh "$root/scripts/install.sh")
m=$work/c
tty_install "$m" "${installer[@]}" || fail "TTY install: $(cat "$m.out")"
grep -q '选择 agent 接入方式' "$m.out" && [[ "$(field "$m/cfg/setup.json" asked)" == True ]] || fail "setup did not run on a terminal: $(cat "$m.out")"
pass "install.sh (TTY): uniflo setup ran (answered 'skip', recorded asked=true in setup.json)"
m=$work/d
tty_install "$m" "${installer[@]}" --no-setup || fail "TTY --no-setup install"
grep -q '跳过 uniflo setup' "$m.out" && ! grep -q '选择 agent 接入方式' "$m.out" && [[ ! -e $m/cfg/setup.json ]] || fail "--no-setup ran setup"
pass "install.sh (TTY, --no-setup): setup skipped"
m=$work/e
tty_install "$m" UNIFLO_NO_SETUP=1 "${installer[@]}" || fail "TTY UNIFLO_NO_SETUP install"
grep -q '跳过 uniflo setup' "$m.out" && ! grep -q '选择 agent 接入方式' "$m.out" && [[ ! -e $m/cfg/setup.json ]] || fail "UNIFLO_NO_SETUP=1 ran setup"
pass "install.sh (TTY, UNIFLO_NO_SETUP=1): setup skipped"

# --- binary self-upgrade ---------------------------------------------------------------------
m=$work/a
before=$(sha "$m/bin/uniflo")
cp "$sums_new" "$work/sums.bak"
printf '%s  uniflo-%s-%s.tar.gz\n' "$(printf '0%.0s' $(seq 64))" "$new_v" "$target" >"$sums_new"
if in_machine "$m" "$m/bin/uniflo" update </dev/null >"$work/f.out" 2>&1; then fail "upgrade with a wrong checksum succeeded"; fi
[[ "$(sha "$m/bin/uniflo")" == "$before" && "$(field "$m/cfg/install.json" version)" == "$old_v" ]] || fail "executable or record changed"
[[ "$(ls -A "$m/bin")" == uniflo ]] || fail "staged files left: $(ls -A "$m/bin")"
pass "uniflo update (wrong checksum): non-zero exit, executable and install.json unchanged"
sed 's/^/      /' "$work/f.out"
cp "$work/sums.bak" "$sums_new"
in_machine "$m" "$m/bin/uniflo" update </dev/null >"$work/g.out" 2>&1 || fail "upgrade: $(cat "$work/g.out")"
[[ "$("$m/bin/uniflo" --version)" == "uniflo $new_v" && "$(sha "$m/bin/uniflo")" == "$(sha "$new_bin")" ]] || fail "not upgraded"
[[ "$(field "$m/cfg/install.json" version)" == "$new_v" ]] || fail "install.json version not updated"
grep -q '已重启' "$work/g.out" && fail "launchd service restarted for a temporary install"
pass "uniflo update (binary): replaced by uniflo $new_v, install.json version=$new_v, launchd service untouched"
sed 's/^/      /' "$work/g.out"

# --- install method ------------------------------------------------------------------------
m=$work/h
mkdir -p "$m/cargo/bin" "$m/other"
cp "$old_bin" "$m/cargo/bin/uniflo"
cp "$old_bin" "$m/other/uniflo"
# exit code 10 = update available, so the JSON is read from a file
method() {
  "$@" >"$work/check.json" 2>>"$work/check.err" || true
  python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["method"])' "$work/check.json"
}
c=$(method in_machine "$m" CARGO_HOME="$m/cargo" "$m/cargo/bin/uniflo" update --check --json)
b=$(method in_machine "$work/a" "$work/a/bin/uniflo" update --check --json)
: >"$work/check.err"
u=$(method in_machine "$m" "$m/other/uniflo" update --check --json)
[[ $c == cargo && $b == binary && $u == unknown ]] || fail "methods: cargo=$c binary=$b unknown=$u"
grep -q 'cargo install uniflo --force' "$work/check.err" && grep -q 'install.sh | sh' "$work/check.err" || fail "unknown hint"
pass "update --check --json: method cargo=$c, binary=$b, unknown=$u; unknown prints both ways (stderr):"
sed 's/^/      /' "$work/check.err"
echo "dist-e2e: all checks passed"
