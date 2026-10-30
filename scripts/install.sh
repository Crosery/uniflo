#!/bin/sh
# Install a prebuilt uniflo release on macOS / Linux (ADR-0009): pick the target, download the
# package and SHA256SUMS, verify, install, record <config dir>/install.json for `uniflo update`,
# then offer `uniflo setup` when run from a terminal.
#
#   curl -fsSL https://github.com/Crosery/uniflo/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/Crosery/uniflo/releases/latest/download/install.sh | sh -s -- --no-setup
#
# Environment:
#   UNIFLO_LANG=zh|en        message language (default: the system language)
#   UNIFLO_VERSION           version to install (default: the newest stable release)
#   UNIFLO_INSTALL_DIR       where the executable goes (default: ~/.local/bin)
#   UNIFLO_NO_SETUP=1        never run `uniflo setup` (same as --no-setup)
#   UNIFLO_RELEASE_BASE_URL  release root (default: GitHub, then ghfast.top and gh-proxy.com in front of it)
#   UNIFLO_CONFIG_DIR, UNIFLO_HOME  where install.json goes, exactly as for uniflo itself
set -eu

# Message language: the first of these that is set and non-empty decides, same rule as the CLI
# (crates/uniflo-cli/src/i18n.rs) -- UNIFLO_LANG, LC_ALL, LC_MESSAGES, the desktop language, then
# LANG. A value starting with "zh" (any case) means Chinese; anything else, including unset, means
# English.
desktop_language() {
  if [ "$(uname -s)" = Darwin ]; then
    defaults read -g AppleLanguages 2>/dev/null | sed -n '2p' |
      sed 's/^[[:space:]]*"\{0,1\}//; s/"\{0,1\},\{0,1\}[[:space:]]*$//'
  fi
}
lang_zh=0
for lang_tag in "${UNIFLO_LANG:-}" "${LC_ALL:-}" "${LC_MESSAGES:-}" "$(desktop_language)" "${LANG:-}"; do
  lang_tag=$(printf '%s' "$lang_tag" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
  if [ -n "$lang_tag" ]; then
    case $lang_tag in [Zz][Hh]*) lang_zh=1 ;; esac
    break
  fi
done
# pick "中文" "English": the message in the detected language.
pick() { if [ "$lang_zh" = 1 ]; then printf '%s' "$1"; else printf '%s' "$2"; fi; }

# Release roots, tried in order: GitHub, then public accelerators in front of it, which answer from
# mainland China when GitHub does not. UNIFLO_RELEASE_BASE_URL replaces them all.
github=https://github.com/Crosery/uniflo/releases
if [ -n "${UNIFLO_RELEASE_BASE_URL:-}" ]; then
  bases=${UNIFLO_RELEASE_BASE_URL%/}
else
  bases="$github https://ghfast.top/$github https://gh-proxy.com/$github"
fi

say() { printf 'uniflo-install: %s\n' "$*"; }
die() {
  if [ "$lang_zh" = 1 ]; then
    printf 'uniflo-install: 错误：%s\n' "$*" >&2
  else
    printf 'uniflo-install: error: %s\n' "$*" >&2
  fi
  exit 1
}

no_setup=0
for arg in "$@"; do
  case $arg in
    --no-setup) no_setup=1 ;;
    *) die "$(pick "未知参数 ${arg}（只支持 --no-setup）" "unknown argument ${arg} (only --no-setup is supported)")" ;;
  esac
done
case ${UNIFLO_NO_SETUP:-} in '' | 0) ;; *) no_setup=1 ;; esac

os=$(uname -s)
arch=$(uname -m)
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "$(pick "没有 $arch 架构的预编译包，可改用 cargo install uniflo" "no prebuilt package for the $arch architecture; use cargo install uniflo instead")" ;;
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
  *) die "$(pick "不支持 ${os}：Windows 请用 install.ps1，其他系统可改用 cargo install uniflo" "unsupported ${os}: use install.ps1 on Windows, or cargo install uniflo on other systems")" ;;
esac

command -v curl >/dev/null 2>&1 || die "$(pick "需要 curl" "curl is required")"
if command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
else
  die "$(pick "需要 shasum 或 sha256sum 来校验下载" "shasum or sha256sum is required to verify the download")"
fi
# fetch <path under the release root> <dest>: the first root that serves it wins; a root that
# cannot connect within 8 s or stalls below 10 KB/s for 20 s hands over to the next.
fetch() {
  for b in $bases; do
    curl -fsSL --connect-timeout 8 --speed-limit 10240 --speed-time 20 -o "$2" "$b/$1" && return 0
    rm -f "$2"
  done
  die "$(pick "下载失败：$1" "download failed: $1")"
}

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t uniflo) || die "$(pick "无法创建临时目录" "could not create a temp directory")"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

version=${UNIFLO_VERSION:-}
version=${version#v}
if [ -z "$version" ]; then
  # The newest stable release's SHA256SUMS names its packages, and so its version.
  fetch "latest/download/SHA256SUMS" "$tmp/latest-SHA256SUMS"
  version=$(sed -n "s/^[0-9a-fA-F]\{64\}[ *]*uniflo-\(.*\)-$target\.tar\.gz\$/\1/p" "$tmp/latest-SHA256SUMS" | head -n 1)
  [ -n "$version" ] || die "$(pick "最新发布里没有 $target 的包" "the latest release has no package for $target")"
fi

pkg=uniflo-$version-$target.tar.gz
say "$(pick "下载 uniflo ${version}（${target}）" "downloading uniflo ${version} (${target})")"
fetch "download/v$version/$pkg" "$tmp/$pkg"
fetch "download/v$version/SHA256SUMS" "$tmp/SHA256SUMS"
want=$(awk -v f="$pkg" '$2 == f || $2 == "*" f { print tolower($1); exit }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "$(pick "SHA256SUMS 里没有 $pkg" "SHA256SUMS has no entry for $pkg")"
got=$(sha256 "$tmp/$pkg" | tr 'A-F' 'a-f')
[ "$got" = "$want" ] || die "$(pick "$pkg 校验失败：SHA-256 为 ${got}，SHA256SUMS 记录的是 ${want}。已中止，没有安装任何文件" "$pkg failed verification: SHA-256 is ${got}, SHA256SUMS records ${want}. Aborted, nothing was installed")"

mkdir "$tmp/unpacked"
tar -xzf "$tmp/$pkg" -C "$tmp/unpacked" || die "$(pick "解包 $pkg 失败" "failed to unpack $pkg")"
new=$tmp/unpacked/uniflo-$version-$target/uniflo
[ -f "$new" ] || die "$(pick "$pkg 里没有 uniflo-$version-$target/uniflo" "$pkg has no uniflo-$version-$target/uniflo")"
reported=$("$new" --version 2>/dev/null || true)
[ "$reported" = "uniflo $version" ] || die "$(pick "解出的 uniflo 无法运行或版本不符（输出：\"$reported\"）" "the unpacked uniflo could not run or reported the wrong version (output: \"$reported\")")"

given_dir=${UNIFLO_INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$given_dir" || die "$(pick "无法创建 $given_dir" "could not create $given_dir")"
dir=$(cd "$given_dir" && pwd -P)
dest=$dir/uniflo
# Staged in the same directory, then renamed: atomic, and fine while an old uniflo is running.
staged=$dir/.uniflo-new-$$
if ! { cp "$new" "$staged" && chmod 755 "$staged" && mv -f "$staged" "$dest"; }; then
  rm -f "$staged"
  die "$(pick "无法写入 $dest" "could not write $dest")"
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
mkdir -p "$cfg" || die "$(pick "无法创建 $cfg" "could not create $cfg")"
record=$cfg/install.json
if ! { printf '{\n  "method": "binary",\n  "target": "%s",\n  "version": "%s",\n  "path": "%s"\n}\n' \
  "$target" "$version" "$(json_str "$dest")" >"$record.tmp.$$" && mv -f "$record.tmp.$$" "$record"; }; then
  rm -f "$record.tmp.$$"
  die "$(pick "无法写入 $record" "could not write $record")"
fi

dest_version=$("$dest" --version)
say "$(pick "已安装 ${dest_version} → $dest" "installed ${dest_version} -> $dest")"
case ":$PATH:" in
  *":$dir:"* | *":$given_dir:"*)
    found=$(command -v uniflo 2>/dev/null || true)
    if [ -n "$found" ] && [ "$found" != "$dest" ] && [ "$found" != "$given_dir/uniflo" ]; then
      say "$(pick "注意：PATH 里先找到的是 ${found}，不是刚装的这一份" "note: PATH finds ${found} first, not the one just installed")"
    fi
    ;;
  *)
    say "$(pick "$given_dir 不在 PATH 中。把下面这行加进 shell 配置（如 ~/.zshrc、~/.bashrc）后重开终端：" "$given_dir is not on PATH. Add the following line to your shell config (e.g. ~/.zshrc, ~/.bashrc) and reopen the terminal:")"
    # shellcheck disable=SC2016 # $PATH stays literal for the user's shell config
    printf '    export PATH="%s:$PATH"\n' "$given_dir"
    ;;
esac

if [ "$no_setup" = 0 ] && [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
  say "$(pick "运行 uniflo setup：把 MCP / Skill 接入本机 agent（之后可用 uniflo setup --uninstall 撤销）" "running uniflo setup: wires MCP / Skill into local agents (undo any time with uniflo setup --uninstall)")"
  "$dest" setup </dev/tty || say "$(pick "uniflo setup 没有完成，之后可以随时运行 uniflo setup" "uniflo setup did not finish; you can run uniflo setup again any time")"
else
  say "$(pick "跳过 uniflo setup；之后可运行 uniflo setup 把 MCP / Skill 接入本机 agent" "skipping uniflo setup; run uniflo setup later to wire MCP / Skill into local agents")"
fi
