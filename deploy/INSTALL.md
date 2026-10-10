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
mkdir -p /home/ubuntu/projects /home/ubuntu/.local/state/dev-session-mcp-v3
chmod 0700 /home/ubuntu/.local/state/dev-session-mcp-v3
```

**Every frontend must use the same `DEV_SESSION_MCP_STATE_DIR`** (and the same `DEV_SESSION_MCP_FILE_ORIGINS` / `DEV_SESSION_MCP_PROFILES`, since whichever frontend starts the broker passes them on). Then stdio and HTTP clients share one broker, one set of sessions and one Events store. Any number of frontends may run at once.

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

2. Create the tunnel, DNS record and Access application with [cloudflare/setup.sh](cloudflare/setup.sh) (run anywhere with `curl` and `jq`; it is idempotent and deletes nothing). It uses the existing Zero Trust organization and admits only the existing Access group `プロジェクトオーナー` (override with `ALLOW_GROUP`, or add `ALLOW_EMAILS`):

   ```sh
   export CLOUDFLARE_API_TOKEN=...   # Account: Cloudflare Tunnel Edit, Access: Apps and Policies Edit,
                                     # Access: Organizations, Identity Providers, and Groups Read; Zone: DNS Edit
   export CLOUDFLARE_ACCOUNT_ID=... MCP_HOSTNAME=mcp.example.com
   # ALLOW_GROUP defaults to the existing Access group プロジェクトオーナー
   DRY_RUN=1 deploy/cloudflare/setup.sh          # preview: only GET requests
   deploy/cloudflare/setup.sh                    # apply; writes ./dev-session-mcp.tunnel-token (0600)
   ```

   It enables Access **Managed OAuth** with dynamic client registration (localhost/loopback redirects for Claude Code and similar CLIs, plus Claude's connector callbacks; add others such as ChatGPT's via `REDIRECT_URIS`). If an Access application already exists for the hostname, its other policies are replaced by the owners policy.

3. On the VPS, install cloudflared 2025.4.0+ from Cloudflare's package repository, then run the tunnel:

   ```sh
   sudo install -d -m 0755 /etc/cloudflared
   sudo install -m 0600 dev-session-mcp.tunnel-token /etc/cloudflared/dev-session-mcp.token
   shred -u dev-session-mcp.tunnel-token
   sudo install -m 0644 deploy/cloudflare/cloudflared-dev-session-mcp.service /etc/systemd/system/
   sudo systemctl daemon-reload && sudo systemctl enable --now cloudflared-dev-session-mcp.service
   ```

   Re-run `setup.sh`: its last line should report that an unauthenticated request is rejected by Access.

4. Add `https://mcp.example.com/mcp` as a custom connector (claude.ai: Settings → Connectors; Claude Code: `claude mcp add --transport http vps https://mcp.example.com/mcp`). The client opens an Access login in the browser. If a client sends a browser `Origin` header, allow it with `--allowed-origin` on the HTTP frontend.

`--allow-remote-bind` exists only for proxies that cannot reach loopback; never expose the port directly.

## Option C: OpenAI Secure MCP Tunnel (stdio)

Use the [official tunnel-client](https://github.com/openai/tunnel-client/releases/latest) (verify SHA256SUMS) with [tunnel-client.yaml.example](tunnel-client.yaml.example) and [dev-session-mcp-tunnel.service](dev-session-mcp-tunnel.service). Place the existing runtime key root-owned 0600 at `/etc/dev-session-mcp-tunnel/runtime-key`; systemd `LoadCredential` supplies it and the fixed `env -i` launcher keeps it out of the frontend and all executions. One active client per Tunnel ID. This can run alongside Options A and B.

## Services and the broker

`KillMode=process` in both units keeps the broker (and running work) alive when a frontend service restarts. Updating the binary does not replace a running broker: a frontend refuses to talk to a broker of a different protocol version and says so. To switch, let executions finish, then stop the old broker (`kill $(cat ~/.local/state/dev-session-mcp-v3/broker.pid)`) — the next request starts the new one. Broker or host restart ends running executions; their records remain with `outcome_unknown`.

## Events and attachments

Events need no extra service: the client supplies the `whsec_` signing secret in `events/subscribe`, and the broker delivers signed webhooks to public HTTPS:443 callbacks. The private state directory stores those secrets. See [EVENTS](../docs/EVENTS.md).

`import_file` is denied until `DEV_SESSION_MCP_FILE_ORIGINS` lists the exact HTTPS origins your client uses for file downloads. See [FILE-TRANSFER](../docs/FILE-TRANSFER.md).
