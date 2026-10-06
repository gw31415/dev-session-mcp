#!/bin/sh
# Read-only checks. Never changes OS/network/auth settings or starts a service.
set -eu
os_name=$(uname -s)
cpu_name=$(uname -m)
test "$os_name" = Linux || { printf 'Linux is required; detected %s\n' "$os_name" >&2; exit 1; }
case "$cpu_name" in
  aarch64|arm64) tunnel_arch=arm64 ;;
  x86_64|amd64) tunnel_arch=amd64 ;;
  *) printf 'Unsupported CPU: %s\n' "$cpu_name" >&2; exit 1 ;;
esac
printf 'Detected OS/CPU: %s %s; official tunnel artifact: linux-%s\n' "$os_name" "$cpu_name" "$tunnel_arch"
if test -r /etc/os-release; then sed -n '/^PRETTY_NAME=/p' /etc/os-release; fi
for executable in node npm tmux bwrap cargo; do command -v "$executable" >/dev/null || { printf 'Missing: %s\n' "$executable" >&2; exit 1; }; done
node -e 'if(Number(process.versions.node.split(".")[0])<22)process.exit(1);console.log("Node "+process.version+" "+process.platform+"/"+process.arch)'
tmux -V
bwrap --version
cargo --version
