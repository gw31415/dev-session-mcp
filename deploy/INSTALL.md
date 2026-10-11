# Installation

Example for an existing Ubuntu account (`ubuntu`); adapt paths. No new UID or sudoers rule is needed. The endpoint is personal: host commands run with that user's full rights.

## Build

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
cargo test --locked --manifest-path rust/Cargo.toml
python3 tests/e2e.py rust/target/release/dev-session-mcp
```

## Install the binary and launchers

```sh
sudo install -d -m 0755 /opt/dev-session-mcp/bin /usr/local/libexec
sudo install -m 0755 rust/target/release/dev-session-mcp /opt/dev-session-mcp/bin/dev-session-mcp
sudo install -m 0755 deploy/dev-session-mcp-stdio /usr/local/libexec/dev-session-mcp-stdio
mkdir -p /home/ubuntu/.local/state/dev-session-mcp-v3
chmod 0700 /home/ubuntu/.local/state/dev-session-mcp-v3
```

**Every frontend must use the same `DEV_SESSION_MCP_STATE_DIR`** (and the same `DEV_SESSION_MCP_FILE_ORIGINS` / `DEV_SESSION_MCP_PROFILES`, since whichever frontend starts the broker passes them on). Then stdio and HTTP clients share one broker, one set of sessions and one Events store. Any number of frontends may run at once.

## Allow the sandbox on hosts that restrict user namespaces

Ubuntu 24.04 and later set `kernel.apparmor_restrict_unprivileged_userns=1`. There the `sandbox` profile and `write_file` fail with `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted` (or `setting up uid map: Permission denied`). Install the profile that grants `userns` to the installed executable only:

```sh
cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns   # 1 = restricted
sudo install -m 0644 deploy/apparmor/dev-session-mcp /etc/apparmor.d/dev-session-mcp
sudo apparmor_parser -r /etc/apparmor.d/dev-session-mcp
```

AppArmor attaches by the resolved executable path, so keep the binary under `/opt/dev-session-mcp/bin/` or `/opt/dev-session-mcp/releases/<id>/` (or edit the pattern). The profile applies to processes started after it is loaded. Restart the frontends and the broker, which runs `sandbox` and `write_file`. A process confined by it shows `dev-session-mcp (unconfined)` in `/proc/<pid>/attr/current`.

## Option A: stdio (Claude Code over SSH, local clients)

```sh
claude mcp add --scope user vps -- ssh -T vps /usr/local/libexec/dev-session-mcp-stdio
```

For a dedicated key, restrict it in `~/.ssh/authorized_keys` on the host:

```
command="/usr/local/libexec/dev-session-mcp-stdio",restrict ssh-ed25519 AAAA... dev-session-mcp
```

## Option B: Streamable HTTP behind Cloudflare Tunnel + Access (claude.ai, Desktop, mobile, ChatGPT, any remote client)

The server has **no authentication**. It binds loopback by default, rejects unknown `Host` headers and any browser `Origin`, and must be published only through an authenticating proxy.

1. Run the HTTP frontend as a service:

   ```sh
   sudo install -m 0644 deploy/dev-session-mcp-http.service /etc/systemd/system/
   # edit --allowed-host to your public hostname
   sudo systemctl daemon-reload
   sudo systemctl enable --now dev-session-mcp-http.service
   curl -fsS http://127.0.0.1:8808/healthz
   ```

2. In Cloudflare (dashboard, API or the `cf` CLI), create:
   - a remotely-managed tunnel with ingress `mcp.example.com` → `http://127.0.0.1:8808` (then `http_status:404`), and a proxied CNAME `mcp.example.com` → `<tunnel-id>.cfargotunnel.com`;
   - a self-hosted Access application for `mcp.example.com` whose only policy admits the owners (for example an existing reusable policy), with **Managed OAuth** and dynamic client registration enabled: allow localhost and loopback redirects (Claude Code and similar CLIs) and the allowed redirect URIs `https://claude.ai/api/mcp/auth_callback` and `https://claude.com/api/mcp/auth_callback` (add others, such as ChatGPT's, as needed).

3. On the VPS, install cloudflared 2025.4.0+ from Cloudflare's package repository, store the tunnel token root-owned 0600 at `/etc/cloudflared/dev-session-mcp.token` without leaving copies elsewhere (for example pipe it over SSH into `sudo install -m 0600 /dev/stdin ...`), then run the tunnel:

   ```sh
   sudo install -m 0644 deploy/cloudflare/cloudflared-dev-session-mcp.service /etc/systemd/system/
   sudo systemctl daemon-reload && sudo systemctl enable --now cloudflared-dev-session-mcp.service
   ```

   Check that an unauthenticated `curl -X POST https://mcp.example.com/mcp` is rejected by Access (401/403).

4. Add `https://mcp.example.com/mcp` as a custom connector (claude.ai: Settings → Connectors; Claude Code: `claude mcp add --transport http vps https://mcp.example.com/mcp`). The client opens an Access login in the browser. If a client sends a browser `Origin` header, allow it with `--allowed-origin` on the HTTP frontend.

`--allow-remote-bind` exists only for proxies that cannot reach loopback; never expose the port directly.

## Option C: OpenAI Secure MCP Tunnel (stdio)

Use the [official tunnel-client](https://github.com/openai/tunnel-client/releases/latest) (verify SHA256SUMS) with [tunnel-client.yaml.example](tunnel-client.yaml.example) and [dev-session-mcp-tunnel.service](dev-session-mcp-tunnel.service). Place the existing runtime key root-owned 0600 at `/etc/dev-session-mcp-tunnel/runtime-key`; systemd `LoadCredential` supplies it and the fixed `env -i` launcher keeps it out of the frontend and all executions. One active client per Tunnel ID. This can run alongside Options A and B.

## Services and the broker

`KillMode=process` in both units keeps the broker (and running work) alive when a frontend service restarts. Updating the binary does not replace a running broker: a frontend refuses to talk to a broker of a different protocol version and says so. To switch, let executions finish, then stop the old broker (`kill $(cat ~/.local/state/dev-session-mcp-v3/broker.pid)`) — the next request starts the new one. Broker or host restart ends running executions; their records remain with `outcome_unknown`.

## Events and attachments

Events need no extra service: the client supplies the `whsec_` signing secret in `events/subscribe`, and the broker delivers signed webhooks to public HTTPS:443 callbacks. The private state directory stores those secrets. See [EVENTS](../docs/EVENTS.md).

`import_file` is denied until `DEV_SESSION_MCP_FILE_ORIGINS` lists the exact HTTPS origins your client uses for file downloads. See [FILE-TRANSFER](../docs/FILE-TRANSFER.md).

### claude.ai attachments (create_upload)

claude.ai does not pass attachments to `import_file`, so they would otherwise have to be pasted through the conversation as base64. `create_upload` instead hands out a single-use URL, and the chat's code-execution sandbox sends the file there with `curl -T`.

1. Add `--upload-base-url https://mcp.example.com` to the HTTP frontend's `ExecStart` (see [dev-session-mcp-http.service](dev-session-mcp-http.service)) and restart it. Without the flag, the tool and the `/upload/` route don't exist.
2. In Cloudflare Access, add a second self-hosted application for the path `mcp.example.com/upload/` with a single **Bypass** policy (Everyone). The sandbox's curl cannot do the OAuth login. Each URL is unguessable, single-use, expires after 15 minutes and is only issued through the authenticated `/mcp`. Leave `/mcp` itself protected. Check that `curl -X POST https://mcp.example.com/mcp` is still rejected and `curl -X PUT https://mcp.example.com/upload/x` returns 404 from the server.
3. In claude.ai, under Settings → Capabilities → code execution, allow network egress to `mcp.example.com` (add it as an allowed domain). Otherwise the sandbox's curl is blocked.

Cloudflare's plan limit on request bodies (100 MB on Free/Pro) applies on top of the server's 128 MiB limit.
