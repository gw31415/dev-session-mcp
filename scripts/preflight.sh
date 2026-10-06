#!/bin/sh
# Read-only platform/dependency checks; no install/network/auth/service changes.
set -eu
os_name=$(uname -s)
cpu_name=$(uname -m)
test "$os_name" = Linux || { printf 'Linux is required; detected %s\n' "$os_name" >&2; exit 1; }
case "$cpu_name" in
  aarch64|arm64) runtime_arch=arm64 ;;
  x86_64|amd64) runtime_arch=amd64 ;;
  *) printf 'Unsupported CPU: %s\n' "$cpu_name" >&2; exit 1 ;;
esac
printf 'Detected OS/CPU: %s %s (%s native build)\n' "$os_name" "$cpu_name" "$runtime_arch"
if test -r /etc/os-release; then sed -n '/^PRETTY_NAME=/p' /etc/os-release; fi
for executable in tmux bwrap; do command -v "$executable" >/dev/null || { printf 'Missing runtime dependency: %s\n' "$executable" >&2; exit 1; }; done
tmux -V
bwrap --version
if test "${1:-}" = --build; then
  for executable in cargo rustc cc make perl pkg-config; do command -v "$executable" >/dev/null || { printf 'Missing build dependency: %s\n' "$executable" >&2; exit 1; }; done
  cargo --version
  rustc --version
fi
printf 'OAuth AS and public HTTPS are configured separately; no change made.\n'
