#!/bin/sh
# Install a prebuilt uniflo release on macOS / Linux (ADR-0009): pick the target, download the
# package and SHA256SUMS, verify, install, record <config dir>/install.json for `uniflo update`,
# then offer `uniflo setup` when run from a terminal.
#
#   curl -fsSL https://github.com/Crosery/uniflo/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/Crosery/uniflo/releases/latest/download/install.sh | sh -s -- --no-setup
#
# Environment:
#   UNIFLO_VERSION           version to install (default: the newest stable release)
#   UNIFLO_INSTALL_DIR       where the executable goes (default: ~/.local/bin)
#   UNIFLO_NO_SETUP=1        never run `uniflo setup` (same as --no-setup)
#   UNIFLO_RELEASE_BASE_URL  release root (default: https://github.com/Crosery/uniflo/releases)
#   UNIFLO_CONFIG_DIR, UNIFLO_HOME  where install.json goes, exactly as for uniflo itself
set -eu

base=${UNIFLO_RELEASE_BASE_URL:-https://github.com/Crosery/uniflo/releases}
base=${base%/}

say() { printf 'uniflo-install: %s\n' "$*"; }
die() {
  printf 'uniflo-install: 错误：%s\n' "$*" >&2
  exit 1
}

no_setup=0
for arg in "$@"; do
  case $arg in
    --no-setup) no_setup=1 ;;
    *) die "未知参数 $arg（只支持 --no-setup）" ;;
  esac
done
case ${UNIFLO_NO_SETUP:-} in '' | 0) ;; *) no_setup=1 ;; esac

os=$(uname -s)
arch=$(uname -m)
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "没有 $arch 架构的预编译包，可改用 cargo install uniflo" ;;
esac
case $os in
  Darwin)
    # A shell under Rosetta reports x86_64 on Apple silicon; install the native build.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
      arch=aarch64
    fi
    target=$arch-apple-darwin
    ;;
  Linux) target=$arch-unknown-linux-musl ;;
  *) die "不支持 $os：Windows 请用 install.ps1，其他系统可改用 cargo install uniflo" ;;
esac

command -v curl >/dev/null 2>&1 || die "需要 curl"
if command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
else
  die "需要 shasum 或 sha256sum 来校验下载"
fi
fetch() { curl -fsSL --retry 2 -o "$2" "$1" || die "下载失败：$1"; }

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t uniflo) || die "无法创建临时目录"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

version=${UNIFLO_VERSION:-}
version=${version#v}
if [ -z "$version" ]; then
  # The newest stable release's SHA256SUMS names its packages, and so its version.
  fetch "$base/latest/download/SHA256SUMS" "$tmp/latest-SHA256SUMS"
  version=$(sed -n "s/^[0-9a-fA-F]\{64\}[ *]*uniflo-\(.*\)-$target\.tar\.gz\$/\1/p" "$tmp/latest-SHA256SUMS" | head -n 1)
  [ -n "$version" ] || die "最新发布里没有 $target 的包"
fi

pkg=uniflo-$version-$target.tar.gz
say "下载 uniflo $version（$target）"
fetch "$base/download/v$version/$pkg" "$tmp/$pkg"
fetch "$base/download/v$version/SHA256SUMS" "$tmp/SHA256SUMS"
want=$(awk -v f="$pkg" '$2 == f || $2 == "*" f { print tolower($1); exit }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "SHA256SUMS 里没有 $pkg"
got=$(sha256 "$tmp/$pkg" | tr 'A-F' 'a-f')
[ "$got" = "$want" ] || die "$pkg 校验失败：SHA-256 为 $got，SHA256SUMS 记录的是 $want。已中止，没有安装任何文件"

mkdir "$tmp/unpacked"
tar -xzf "$tmp/$pkg" -C "$tmp/unpacked" || die "解包 $pkg 失败"
new=$tmp/unpacked/uniflo-$version-$target/uniflo
[ -f "$new" ] || die "$pkg 里没有 uniflo-$version-$target/uniflo"
reported=$("$new" --version 2>/dev/null || true)
[ "$reported" = "uniflo $version" ] || die "解出的 uniflo 无法运行或版本不符（输出：\"$reported\"）"

given_dir=${UNIFLO_INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$given_dir" || die "无法创建 $given_dir"
dir=$(cd "$given_dir" && pwd -P)
dest=$dir/uniflo
# Staged in the same directory, then renamed: atomic, and fine while an old uniflo is running.
staged=$dir/.uniflo-new-$$
if ! { cp "$new" "$staged" && chmod 755 "$staged" && mv -f "$staged" "$dest"; }; then
  rm -f "$staged"
  die "无法写入 $dest"
fi

# Same location as uniflo_core::paths::config_dir(): UNIFLO_CONFIG_DIR as is, otherwise the
# platform directory, re-rooted under UNIFLO_HOME at the same home-relative path.
config_dir() {
  if [ -n "${UNIFLO_CONFIG_DIR:-}" ]; then
    printf '%s\n' "$UNIFLO_CONFIG_DIR"
    return
  fi
  case $os in
    Darwin) cfg_base="$HOME/Library/Application Support" ;;
    *)
      cfg_base=${XDG_CONFIG_HOME:-}
      case $cfg_base in /*) ;; *) cfg_base=$HOME/.config ;; esac
      ;;
  esac
  if [ -n "${UNIFLO_HOME:-}" ]; then
    case $cfg_base in
      "$HOME"/*) cfg_base=$UNIFLO_HOME/${cfg_base#"$HOME"/} ;;
      *) cfg_base=$UNIFLO_HOME/.config ;;
    esac
  fi
  printf '%s/uniflo\n' "$cfg_base"
}
json_str() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
cfg=$(config_dir)
mkdir -p "$cfg" || die "无法创建 $cfg"
record=$cfg/install.json
if ! { printf '{\n  "method": "binary",\n  "target": "%s",\n  "version": "%s",\n  "path": "%s"\n}\n' \
  "$target" "$version" "$(json_str "$dest")" >"$record.tmp.$$" && mv -f "$record.tmp.$$" "$record"; }; then
  rm -f "$record.tmp.$$"
  die "无法写入 $record"
fi

say "已安装 $("$dest" --version) → $dest"
case ":$PATH:" in
  *":$dir:"* | *":$given_dir:"*)
    found=$(command -v uniflo 2>/dev/null || true)
    if [ -n "$found" ] && [ "$found" != "$dest" ] && [ "$found" != "$given_dir/uniflo" ]; then
      say "注意：PATH 里先找到的是 $found，不是刚装的这一份"
    fi
    ;;
  *)
    say "$given_dir 不在 PATH 中。把下面这行加进 shell 配置（如 ~/.zshrc、~/.bashrc）后重开终端："
    # shellcheck disable=SC2016 # $PATH stays literal for the user's shell config
    printf '    export PATH="%s:$PATH"\n' "$given_dir"
    ;;
esac

if [ "$no_setup" = 0 ] && [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
  say "运行 uniflo setup：把 MCP / Skill 接入本机 agent（之后可用 uniflo setup --uninstall 撤销）"
  "$dest" setup </dev/tty || say "uniflo setup 没有完成，之后可以随时运行 uniflo setup"
else
  say "跳过 uniflo setup；之后可运行 uniflo setup 把 MCP / Skill 接入本机 agent"
fi
