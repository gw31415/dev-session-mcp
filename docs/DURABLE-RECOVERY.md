# Bounded durable execution recovery

Local implementation for parent review. No live frontend, broker, auth, subscription,
webhook, or service configuration is changed by this commit. Existing executions
must not be interrupted to activate it. Build only in the dev-session target;
VPS rollout and any broker transition remain a separate reviewed operation.

## Contract and compatibility

The server remains MCP 2026-07-28 (`server/discover`, per-request metadata).
Existing tools-only clients keep the original start ACK and default detail read/wait
shape (with additive evidence fields). This does not introduce support for the
2025 initialize handshake, which the existing server already rejected.
No Tasks capability is advertised. The official extension can later adapt the same
records, but a start ACK and a task representing process termination are different
contracts. There is no second process owner or Tasks-specific execution store.

The broker advertises `recovery: 1` in its private ping response. The frontend checks
this before new recovery-dependent operations. An old retained broker must never
silently ignore an idempotency key and launch twice. Legacy default detail operations
still work with the old broker; new recovery requests fail before sending a command.
No live broker is upgraded or restarted automatically to gain these features.

## Short observation and explicit detail

- `read_execution`: optional `view: "summary" | "detail"`; default remains detail.
- `wait_execution`: same view, with optional `state_cursor` **only for summary**.
  Required `cursor` remains the caller's detail position.
- Summary contains no output/source body. `state_cursor` is the observed journal
  position; `detail_cursor` (also the summary's legacy `cursor`) remains the supplied
  detail position. Without a read cursor, summary uses the persisted `initial_cursor`.
- `detail_available`, `catch_up_required`, `earliest_cursor`, and `more` describe the
  bounded detail page. `details_uri` is an explicit resource link represented as a URI
  field. These are hints to fetch detail, not acknowledgements of processing.
- After a summary wait, pass its `state_cursor` to the next summary wait while retaining
  the original detail cursor. Otherwise unread output can cause immediate return again.
- Detail retains non-destructive, repeatable 32 KiB pages and epoch/sequence identities.
  On `more`, drain the returned cursor explicitly. A snapshot can report termination
  before all output pages have been processed.
- Legacy wait uses at most `max_wait_ms + 2000ms` internal transport budget. Summary
  adds a final detail-gap snapshot plus the frontend recovery capability probe, for
  `max_wait_ms + 4000ms`. Scheduling and serialization overhead remain additional.
  A deadline error means retain the previous cursor, not success or completion.
- `durable_cursor` is the execution's most recently committed event position; it is
  distinct from the broker-global observation/detail positions. `persistence_ok` reports
  storage health, not proof that every in-memory output byte is committed.

State/progress is factual process evidence; no guessed percentages, inferred input
wait, or automatic work-completion claim is added. Purpose and completion criteria
are stored metadata, not executable instructions or authorization.

## Start intent and uncertain results

Choose and persist an `idempotency_key` **before** `start_execution`. Optional
`work_id`, `purpose`, and `completion_condition` associate the operation with the work.
Keys are scoped to the project session. An identical keyed retry returns the recorded
execution with `replayed: true`; a changed request using that key is rejected. Omitted
`io` and `io: "pty"` are equivalent; other arguments must be identical. Original argv
is retained for explicit source retrieval, not repeated in normal execution records.

The broker serializes starts, mints an opaque execution ID, and atomically commits
intent before spawning. The intent and key cannot be evicted to make room for a new
command. The process PID/start ticks are saved once available. The pre-spawn recovery
ID survives the unavoidable spawn/record crash window.

A timeout or disconnect is **not** proof that start/input failed. Recover by IDs,
`list_sessions`, `open_session`, or the same previously persisted start key. Never
blindly retry a start without its original key, and never automatically resend stdin.
Read, wait, Resources, and checkpoint operations contain no command spawn path.
An intent without confirmed start/termination evidence returns `outcome_unknown`.
It does not automatically try to start again, even if the crash happened before spawn.
This is duplicate suppression with explicit uncertainty, not an exactly-once execution
claim. There is deliberately no stdin retry key in this change.

## Storage, limits, and recovery

`DEV_SESSION_MCP_STATE_DIR/recovery.json` is new broker-owned state, separate from
existing session metadata and Events subscription storage. A small `recovery.initialized`
marker detects snapshot loss and prevents silently opening a fresh idempotency keyspace.
Production state is not
read by the tests. Writes use temporary file, fsync, rename, and parent-directory fsync.
Intent, lifecycle/input evidence, and checkpoint acknowledgements commit synchronously.
Output is batched by the existing broker process every 250ms (best effort scheduling)
and at lifecycle/checkpoint commits. No new daemon, service, or external database is
introduced. This avoids rewriting the complete retained log for every output chunk.

Limits per state directory:

| Evidence | Bound and behavior |
| --- | --- |
| Execution records / start keys | 128 records; refuse new starts at capacity; no automatic key eviction |
| Reader checkpoints | 128; refuse new reader/work bindings at capacity |
| Durable output history | 1 MiB / 1024 events globally, oldest events removed |
| Recovery snapshot | 4 MiB; atomic replacement can temporarily require another 4 MiB |
| Source argv | Existing 64 KiB serialized command bound |
| Purpose / completion condition | 2048 UTF-8 bytes each |

The existing live process limit remains 64; evicted terminal live records remain
recoverable through the durable store. Metadata growth can also hit the 4 MiB limit
before the count limits. A storage failure disables further starts and checkpoint
writes in that broker lifetime and exposes `persistence_ok: false`; reads remain useful
as evidence. There is no automatic cleanup/reset API that could forget deduplication.
State archival/reset at capacity needs a separately reviewed lifecycle decision.

On broker restart, persisted terminal evidence remains available. `running` or
`starting` becomes `outcome_unknown` with a recovery reason and
`history_tail_unknown: true`: unsaved output and an unrecorded exit may exist.
The broker does not infer death from a changed epoch, reconnect to bare PIDs, or respawn.
Committed older pages remain readable using their original epoch. Ring eviction is
reported with `history_lost` / `catch_up_required` on archived reads; a live read uses
its existing `catch_up_required` flag. A missing snapshot with an initialization marker, or a corrupt snapshot, fails closed.
Removal of the entire state directory (including the marker) cannot be distinguished
from a fresh installation; recovery guarantees require preserving that directory.
Entirely missing/corrupt state cannot establish
an old execution's outcome; errors do not claim failure or success.

## Reader checkpoint CAS and rediscovery

`checkpoint_execution` is the only new tool. Supply `execution_id`, `reader_id`,
`cursor`, and `expected_revision` (0 for creation). Supply purpose/completion condition
on creation unless inherited from start. The key is `(session_id, work_id, reader_id)`.
It stores the processed `detail_cursor`, purpose, completion condition, and a revision.
An existing binding can move to the next execution in the same work through CAS.
Within one execution the cursor cannot move backwards or cross epochs; global sequence
gaps caused by other executions remain valid. Successful read alone never writes it.

Two readers have independent acknowledgements. CAS conflict, lost checkpoint ACK, or
uncertain storage requires reading the current record and reconciling; it does not
justify re-executing a command or treating unprocessed detail as acknowledged.
The server validates position/revision, while the reader attests that it processed the
page. A checkpoint neither certifies the completion condition nor schedules more work.

`list_sessions` and reopening the same project with `open_session` return executions
and checkpoints, including archived/unknown records. Thus recovery does not depend
on remembering a handle only in conversation text. `open_session` preserves its
existing id/cwd fields and adds recovery arrays.

## Resources and official MCP alignment

- `dev-session:///recovery`: same read-only recovery index as `list_sessions`.
- `dev-session:///executions/{execution_id}{?cursor,view}`: same broker read/project
  implementation as `read_execution`, including bounded pages and gaps.
- `dev-session:///source/{execution_id}`: original argv only on explicit request.

Lists/templates/read provide `ttlMs: 0` and `cacheScope: "private"`. No resource
subscription is added. Hosts decide how Resources reach their model/UI; tools remain
available for existing consumers.

Primary sources checked on 2026-10-08:

- [Current revision](https://modelcontextprotocol.io/docs/2026-07-28/learn/versioning)
- [2026-07-28 changes](https://modelcontextprotocol.io/specification/2026-07-28/changelog):
  no `tasks/list` or SSE `Last-Event-ID` recovery.
- [Tasks extension](https://github.com/modelcontextprotocol/ext-tasks/blob/main/specification/2026-07-28/tasks.md):
  explicit per-request capability, durable handle before ACK, `completed` includes
  tool `isError`; `failed` is a JSON-RPC error; cancel ACK is not stop confirmation;
  no progress/log notifications on task subscriptions.
- [Progress](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/progress):
  scoped to an active request, not an already acknowledged process launch.
- [Resources](https://modelcontextprotocol.io/specification/2026-07-28/server/resources)
- [Client matrix](https://modelcontextprotocol.io/extensions/client-matrix): no Tasks
  support column at review time; actual client capability remains unconfirmed.

## Regression scope

`durable::tests` covers reserved-intent crash, request mismatch, bounded key retention,
reader CAS, failed writes, summary position separation, durable history truncation,
restart uncertainty, and missing/corrupt snapshot refusal. `tests/recovery_stdio.py` uses a temporary state directory
and short children for dropped start ACK, duplicate concurrent retries, frontend
reconnect, summary/detail/Resource equivalence, two readers and CAS conflict, broker
SIGKILL/restart, unknown outcome, and durable capacity refusal.

Existing wait/stdio/IO/diagnostics tests continue to exercise the default detail
contract. Live client Tasks support, chat delivery, deployment, and production
execution continuity are not claimed by these isolated tests.

## Local validation (2026-10-08)

All tests used temporary state / synthetic data and short local children. The old
broker fixture was the repository's `artifacts/deployment-3d207fc-20261008/dev-session-mcp`,
launched with a temporary socket; no existing broker process was contacted.

| Check | Result |
| --- | --- |
| `cargo test --locked --offline ... -- --test-threads=1` | 17 passed, 0 failed |
| `cargo build --locked --offline ...` | Passed |
| `tests/recovery_stdio.py NEW OLD` | Passed; start/input/checkpoint ACK loss, two readers, Resources including escaped URIs, restart, unknown result, capacity, old-broker guard |
| `tests/wait_execution_stdio.py NEW` | Passed |
| `tests/wait_execution_stdio.py NEW OLD` | Passed against retained 3d207fc fixture |
| `tests/delivery_diagnostics_stdio.py NEW` | Passed |
| `tests/stdio.py NEW` | Passed; existing catalog expectation updated to 16 tools |
| `tests/io_control.py NEW` | Passed |
| `cargo fmt -- --check`, `git diff --check` | Passed |

Build environment: `CARGO_INCREMENTAL=0`, `RUSTC_WRAPPER=`,
`CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target`, `CARGO_BUILD_JOBS=1`.
Builds used a free-disk guard that terminates their own process group at 200 MiB.
About 1290 MiB remained after validation; no Mycast target was modified.

An initial per-output-chunk full snapshot caused a real wait snapshot deadline failure
under a 1.2 MB output fixture. Bounded output batching and incremental history byte
accounting fixed that failure; the same fixture now passes. Output since the last
commit may be lost on crash, so unknown history tails are explicitly reported.
No live schema/auth/subscription/service changes, push, or deployment were performed.
