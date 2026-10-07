# Validation — 2026-10-07

This records version 0.3 source verification in the existing independent dev-session-mcp workspace, on Linux x86_64 / Debian 13, Rust 1.98.1, bubblewrap 0.12.0. No mycast files/config were changed; no production VPS/Tunnel process or session was stopped. No ARM64/OCI verification is claimed for this update.

`cargo build --locked --offline --manifest-path rust/Cargo.toml` succeeded. `cargo test --locked --offline --manifest-path rust/Cargo.toml` succeeded: **4 passed, 0 failed**, 1.65 seconds after compilation. The tests exercise:

- Actual HTTP webhook signing, exact serialized retry body/eventId and new+old key signatures; production rejects the fixture's private callback.
- Actual independent broker execution → signed verification echo → pushed output → failed receipt → frontend delivery-engine restart → exact stored batch retry. Finite lease expiry stops the waiting delivery worker; unsubscribe is idempotent and private store mode is 0600.
- Existing binary attachment stream cap, SHA256, failed-transfer cleanup, atomic no-overwrite/explicit replacement and private URL/origin policy.

`python3 tests/stdio.py <built-binary>` exercises the real stdio endpoint, not a mocked MCP server: discovery Events capability and exact 13-tool catalog, canonical session reopening with no implicit shell, sandbox file write/read and outside-root/symlink denial, structured file truncation, separate stdout/stderr pipes, host PTY stdin/resize/signals, concurrent commands, same-PID frontend reconnect without rerun, input accepted/written/uncertain receipts, bare/stale PID rejection, broker epoch rejection and cursor gap, large-output bounded history/pages, private callback verification failure and session shutdown. [Recorded stdout](evidence/events-stdio-20261007.log).

The initial nested sandbox attempts failed because the outer managed execution sandbox exposes its synthetic mount lock/uid-map locations read-only. A test-only TMPDIR resolved the synthetic mount location, and the real test then ran with automatically approved ordinary Linux execution permissions to exercise Codex/bubblewrap rather than weaken its policy. It only touched temporary test state/projects and managed test children. No host fallback was added.

The compiled runtime allows private loopback HTTP callbacks **only under cfg(test)** for actual local delivery fixtures; no production env/config flag enables them. These tests do not prove OpenAI callback authorization, real ChatGPT/dot Events/catalog integration, actual Tunnel authentication, ARM64 build, release build or systemd placement. Those remain coordinated runtime checks.

Historical evidence files and PUBLICATION-AUDIT describe earlier revisions and are not current API or runtime proof. Current source uses a single broker and journal, explicit profile/ID APIs, stdio MCP 2026 lifecycle, webhook Events and cursor recovery. The production runtime currently exposes its prior catalog until an explicit safe cutover/rescan.
