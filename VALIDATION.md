# Validation — 2026-10-09 (v0.4.0)

Environment: Linux x86_64, Rust 1.97.0, bubblewrap 0.9.0. Temporary state directories only; no production host, tunnel or Cloudflare configuration was touched.

| Check | Result |
| --- | --- |
| `cargo build --release --locked` | OK, no warnings |
| `cargo test --locked` | 16 passed: durable store (corrupt snapshot refusal, intent replay, retirement, pinned work, reader CAS, restart summaries), webhook signing/retry/private-callback refusal, attachment transfer and origin policy, admin profiles, wait race/deadline, HTTP discovery patch, tool catalog |
| `python3 tests/e2e.py <release binary>` | PASS. Two stdio frontends (2026-07-28 `server/discover` and 2025-06-18 `initialize`) and one Streamable HTTP frontend (both lifecycles) connected at once to one broker: shared sessions/executions, Host and Origin rejection, sandbox file writes and execution, pipes, PTY input/resize across a frontend reconnect, pipe EOF, Events listing and private-callback rejection from stdio and HTTP, confirmed session close, HTTP frontend restarting a stopped broker |
| Claude Code 2.1.295 (`claude -p` with `.mcp.json`) | Connected to the HTTP (`type: http`) and stdio servers simultaneously, started a host execution over HTTP, waited for it, and read the same execution ID over stdio |

This run found and fixed one real incompatibility: Claude Code passes `MCP_*` variables (e.g. `MCP_TOOL_TIMEOUT`) to stdio servers, which the old frontend refused. Frontends now accept any environment; the broker and executions still start from a cleared environment.

Not verified here: ARM64 build, Cloudflare Tunnel + Access OAuth with claude.ai connectors, ChatGPT/Codex over the new HTTP endpoint, real Events webhook delivery to OpenAI, and systemd placement on the VPS.
