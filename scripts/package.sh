#!/usr/bin/env bash
# Pack one release binary exactly as the release workflow uploads it (ADR-0009):
#   uniflo-<version>-<target>.tar.gz (Windows: .zip) containing
#   uniflo-<version>-<target>/{uniflo[.exe], LICENSE, THIRD_PARTY_NOTICES.md}
# Usage: scripts/package.sh <version> <target> <binary> <out-dir>   # prints the package path
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ $# -ne 4 ]]; then
  echo "usage: scripts/package.sh <version> <target> <binary> <out-dir>" >&2
  exit 2
fi
version=$1 target=$2 bin=$3
mkdir -p "$4"
out="$(cd "$4" && pwd)"
name="uniflo-$version-$target"
exe=uniflo
[[ $target == *windows* ]] && exe=uniflo.exe

# Staged inside the out dir so the Windows archiver only sees relative paths.
stage="$out/.stage-$name"
rm -rf "$stage"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name"
cp "$bin" "$stage/$name/$exe"
chmod 755 "$stage/$name/$exe"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$stage/$name/"

if [[ $target == *windows* ]]; then
  pkg="$name.zip"
  rm -f "$out/$pkg"
  if command -v 7z >/dev/null; then
    (cd "$stage" && 7z a -tzip -bd -y "../$pkg" "$name" >/dev/null)
  else
    (cd "$stage" && zip -qr "../$pkg" "$name")
  fi
else
  pkg="$name.tar.gz"
  flags=()
  # bsdtar (macOS) would otherwise carry extended attributes and AppleDouble files.
  case "$(tar --version 2>/dev/null)" in *bsdtar*) flags=(--no-xattrs --no-mac-metadata) ;; esac
  COPYFILE_DISABLE=1 tar ${flags[@]+"${flags[@]}"} -czf "$out/$pkg" -C "$stage" "$name"
fi
printf '%s\n' "$out/$pkg"
