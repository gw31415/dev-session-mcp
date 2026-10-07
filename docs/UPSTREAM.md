# local-mcp relationship and maintenance

dev-session-mcp extends ideas and tool implementations from
[nakasyou/local-mcp](https://github.com/nakasyou/local-mcp), by Shotaro Nakamura.
It is an independently maintained development-session MCP server, not an
official upstream release or an endorsed integration.

## Why the base tools are adapted source

The upstream repository currently defines a `local-mcp` executable and a
separate Linux sandbox helper. Its source tree has no `lib.rs` or public
library interface. The official crates.io API for `local-mcp` returned 404
when checked on 2026-10-07. There is no confirmed upstream library crate to
declare as a Cargo dependency.

We therefore retain only the four adapted modules actually used by this
application, under `rust/src/base`, instead of shipping the entire upstream
repository, manifests, lockfile, CLI, and handwritten MCP transport. These
modules are maintained directly as source. Builds do not fetch local-mcp,
apply string replacements, or import a hash-pinned source snapshot. The
origin commit in NOTICE is a provenance record, not a build mechanism.

| Responsibility | Ownership |
| --- | --- |
| Base ten tools, file access, approval requests and session configuration | Derived from local-mcp; adapted in `rust/src/base` |
| Codex sandbox invocation | Derived from local-mcp; Linux helper routed into this executable |
| MCP protocol and transports | Official `rmcp` dependency, composed in `server.rs` |
| Persistent PTYs, private broker, reconnect, output limits, stdin and resize | dev-session-mcp additions in `broker.rs` and `workspace.rs`; `pty-process` dependency |
| Attachment import and URL policy | dev-session-mcp addition in `files.rs` |
| Tunnel credential separation, wrapper and systemd deployment | dev-session-mcp deployment files; official external tunnel-client |
| Optional HTTP/OAuth authorization | dev-session-mcp additions; unused by the recommended Tunnel deployment |

The base tools are `session_info`, `read_file`, `get_image`, `list_directory`,
`write_file`, `execute`, `start_command`, `poll_job`, `stop_job`, and
`without_sandbox`. This project adds ten session/PTY/file-import tools.
See [README usage](../README.md#道具と使い方).

## Dependency and update policy

Cargo dependencies use ordinary package versions and Cargo.lock. Codex's
workspace crates use an explicit git revision because those APIs are used
directly; this is a normal dependency pin, not a copied repository snapshot.
`cargo build --locked` keeps these dependencies reproducible.

When updating the base tools, compare upstream changes against the origin
record and the four maintained modules. Port relevant fixes explicitly,
record the source and reason, preserve notices, and run the real stdio
integration test covering tools, sandboxing and approval behavior. Do not
automatically replace these modules with a whole upstream checkout. If
upstream publishes a supported library interface, reassess using it as a
normal dependency before adding another adapter.

## Attribution and choosing a server

Use [local-mcp's own installation and usage](https://github.com/nakasyou/local-mcp#readme)
for its base local tools and approval workflow. Use this project when the
additional retained PTY sessions and credential-separated Secure MCP Tunnel
deployment fit the requirement. The upstream author remains credited for
the derived code; the additional behavior and its maintenance are this
project's responsibility.

[NOTICE](../NOTICE.md) and [the preserved upstream license](../licenses/local-mcp-MIT.txt)
document the original copyright and the upstream license/manifest discrepancy.
