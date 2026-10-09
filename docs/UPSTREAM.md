# local-mcp relationship and maintenance

dev-session-mcp adapts file-access, session configuration and Codex sandbox invocation from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp), by Shotaro Nakamura. It is independently maintained, not an official upstream release or endorsed integration. [Upstream installation and usage](https://github.com/nakasyou/local-mcp#readme) describes that project's own tools and approval workflow.

## Why the base is adapted source

At the recorded source revision, upstream defines an executable and separate Linux sandbox helper, with no lib.rs/public library API. The official crates.io API returned 404 for local-mcp when checked on 2026-10-07. No confirmed upstream library crate exists to declare as a normal dependency.

This repository maintains three necessary derived modules, `rust/src/base/{config,sandbox,tools}.rs`, directly. It does not ship/fetch/rewrite the complete upstream repository or use its handwritten MCP transport. NOTICE's origin commit is provenance, not a build-time snapshot pin. Source headers and the original MIT notice remain.

| Responsibility | Ownership |
| --- | --- |
| Bounded file tools, project metadata, Codex sandbox invocation | Adapted local-mcp source in rust/src/base |
| MCP lifecycles (2026-07-28 and 2025) over stdio and Streamable HTTP | Official rmcp dependency; small Events discovery adapter; axum listener |
| Single process owner, explicit execution API, bounded journal/cursors, PTY/pipes, input and signal routing | dev-session-mcp broker; pty-process dependency |
| Events verification/signatures, durable bounded outbox, leases/retry | dev-session-mcp events module |
| Attachment import/URL policy | dev-session-mcp files module |
| Same-user launchers and systemd units | dev-session-mcp deploy files; external tunnel-client / cloudflared |

Version 0.3 intentionally replaces the old tool contracts. Upstream approval console, execute/start/poll/stop/without_sandbox and the previous dual job/session maps are removed; no legacy compatibility is claimed. Sandbox restrictions remain explicit in the mandatory execution profile and file write path. Host profile openly grants the owner's normal OS rights. See [README](../README.md).

## Dependency and update policy

Cargo uses normal package versions and Cargo.lock. Codex workspace crates use an explicit Git dependency revision because their APIs are called directly. This is a normal dependency pin, not a copied repository snapshot; cargo build --locked is reproducible.

Compare relevant upstream fixes against the recorded origin and the three maintained modules. Port relevant changes explicitly, record the source/reason, preserve notices and run the real stdio/sandbox checks. Do not auto-replace these files with a complete checkout. If upstream publishes a supported library API, reassess a normal dependency before adding another adapter.

The upstream author remains credited for derived code. Added behavior and maintenance are this project's responsibility. [NOTICE](../NOTICE.md) and [preserved MIT license](../licenses/local-mcp-MIT.txt) record the supplied license and original Cargo metadata discrepancy.
