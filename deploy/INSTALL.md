# Linux installation and safe update

This is an unexecuted example for the existing Ubuntu account, with no new UID or sudoers grant. Adapt all `ubuntu` paths to the actual existing user. The endpoint is personal and trusted: host commands have that user's full rights and can read that user's credential files and Events store. Clean environment inheritance and private file modes do not isolate mutually trusted same-UID processes.

## Build on the actual OS/CPU

Keep this repository independent of other projects. Detect rather than assume architecture:

```sh
uname -sm
cat /etc/os-release
sh scripts/preflight.sh --build
cargo build --locked --manifest-path rust/Cargo.toml
cargo test --locked --manifest-path rust/Cargo.toml
python3 tests/stdio.py rust/target/debug/dev-session-mcp
cargo build --release --locked --manifest-path rust/Cargo.toml
```

Linux ARM64/aarch64 and AMD64/x86_64 use native Rust builds. Rust 1.96+, C compiler, make/perl/pkg-config, bubblewrap and CA certificates are required. Use `CARGO_BUILD_JOBS=1` on a small host. Namespace/sandbox denial is a test failure to diagnose; never fall back to host automatically. The Rust binary contains the Codex Linux sandbox helper and independent broker. No Node, tmux, Mac or GHA is required.

## Existing Tunnel and single-user placement

Use the [official Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels). Get the full client for the detected CPU from [official releases](https://github.com/openai/tunnel-client/releases/latest), verify its published SHA256SUMS, and record the installed version. Linux arm64 and amd64 distributions are provided. Use the existing Tunnel ID/runtime key and existing workspace association; this implementation does not create keys or grants. Outbound API port 443 is required; no inbound shell/MCP port or Tailscale/firewall change is needed. The health listener remains loopback.

The example wrapper is a fixed, argument-free `env -i` launcher. It excludes Tunnel credential environment variables from stdio and all execution children. The Events store contains signing secrets but command environments do not. Review the same-UID trust boundary above before connecting other people or agents.

Unexecuted new-install placement example:

```sh
sudo install -d -o root -g root -m 0755 /opt/dev-session-mcp/bin /usr/local/libexec
sudo install -o root -g root -m 0755 rust/target/release/dev-session-mcp /opt/dev-session-mcp/bin/dev-session-mcp
mkdir -p /home/ubuntu/projects /home/ubuntu/.local/state/dev-session-mcp-v3
chmod 0700 /home/ubuntu/.local/state/dev-session-mcp-v3
sudo install -o root -g root -m 0755 deploy/dev-session-mcp-stdio /usr/local/libexec/dev-session-mcp-stdio
sudo install -d -o root -g root -m 0700 /etc/dev-session-mcp-tunnel
sudo install -o root -g root -m 0600 deploy/tunnel-client.yaml.example /etc/dev-session-mcp-tunnel/config.yaml
sudo install -o root -g root -m 0644 deploy/dev-session-mcp-tunnel.service /etc/systemd/system/dev-session-mcp-tunnel.service
```

Set the real existing Tunnel ID in config.yaml, and place its existing runtime key through a secure local terminal into `/etc/dev-session-mcp-tunnel/runtime-key`, root-owned 0600. Do not paste it into chat, argv, export, shell history or logs. systemd LoadCredential supplies it to the Tunnel process, and the fixed launcher removes inherited credential environment. Same UID means host commands may still read that process's runtime credential copy; no UID isolation is claimed. No sudoers rule is required.

For a new installation only, after confirming no other client owns that Tunnel ID:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now dev-session-mcp-tunnel.service
sudo systemctl status dev-session-mcp-tunnel.service
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

One active client per Tunnel ID. `KillMode=process` preserves the broker through frontend replacement. It does not terminate work or update an already running broker binary. broker.sock is 0600 and peer UID checked; state is 0700. Session metadata and Events subscriptions share this single state directory; output/state journal is broker RAM. Broker termination/host restart loses executions and journal.

## Updating a host currently used for access

Version 0.3 is a breaking API/state change. There is no old-schema migration or legacy shim. Do not stop the live Tunnel/broker while it is the only active control path.

1. Inspect the actual running binary, unit, wrapper, UID, state path and active sessions through the existing connection. Report what was observed; do not assume this example describes the live host.
2. Build the new binary at a separate versioned path. Run the above checks with temporary state, which does not contact the live broker or alter project files.
3. Keep the old binary, configuration and state for rollback. Prepare the new fixed wrapper with a **new state path** such as `dev-session-mcp-v3`; do not point the new binary at the old socket. This lets old executions remain under their old broker while testing the new broker.
4. Coordinate any necessary old session termination and the precise cutover with the user/parent. Maintain the existing SSH/Tailscale recovery path. Do not infer approval to kill active work from approval to implement or push source.
5. At the agreed cutover, replace only the chosen Tunnel MCP command, restart that frontend, and rescan the plugin's catalog. Ensure only one client uses the existing Tunnel ID. The old broker persists until explicitly managed; it is not killed by this source update.
6. Verify `server/discover`, the 13-tool catalog, `open_session`, actual sandbox/host operations and `execution.events` subscription/verification/delivery in ChatGPT/dot. Check stdin receipt and frontend reconnect. Only then report the runtime version as connected and verified.

The current source is saved to main; this implementation session did not modify or stop the production VPS. ARM64/OCI build, actual platform Events callback and Tunnel deployment remain runtime checks.

## Events and attachments

Events need no additional service or manually created key. The client provides the whsec signing secret during authenticated `events/subscribe`. Callback delivery is public HTTPS/443 with verification, DNS/IP checks, signatures, finite leases and bounded retry. Private state storage contains secrets: protect backups and do not print the store. See [EVENTS](../docs/EVENTS.md).

`import_file` remains denied until the fixed wrapper's `DEV_SESSION_MCP_FILE_ORIGINS` is configured with a verified exact HTTPS origin used by the actual client. Do not log signed URLs or allow a model-supplied/wildcard origin. This setting is not passed into command environments. [File transfer details and unimplemented output-side integration](../docs/FILE-TRANSFER.md).
