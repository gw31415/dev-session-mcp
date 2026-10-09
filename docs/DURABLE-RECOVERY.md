# Bounded durable execution recovery

## Contract

The same tools are served over stdio and Streamable HTTP, to 2026-07-28
(`server/discover`) and 2025 (`initialize`) clients alike. Start ACKs and the default
detail read/wait shape are unchanged. No Tasks capability is advertised: a start ACK
and a task representing process termination are different contracts, and there is
no second process owner or Tasks-specific execution store.

Every frontend checks the broker's private protocol number when it connects and
sends nothing to a broker of a different version, so a retained old broker can never
silently ignore an idempotency key and launch twice. No live broker is upgraded or
restarted automatically.

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

For a new operation, read `recovery.key_generation` from `open_session`,
`list_sessions`, or the recovery Resource. Persist that `key_generation` together with
an `idempotency_key` **before** `start_execution`. A retry must use the same pair;
never replace a retired generation with the current one to retry an old operation.
Optional
`work_id`, `purpose`, and `completion_condition` associate the operation with the work.
Keys are scoped to `(project session, generation)`. An identical retained keyed retry
returns the recorded execution with `replayed: true`; a changed request using that key
is rejected. Both first ACK and replay `cursor` refer to the persisted `start_cursor`
before child output, even if the replayed status is already `exited`. Clients can pass
`job.cursor` directly to read/wait; no manual switch to `initial_cursor` is required.
For migrated records lacking `start_cursor`, replay falls back conservatively to
`initial_cursor`. Replay's `observed_cursor` carries the latest state position. Omitted
`io` and `io: "pty"` are equivalent; other arguments must be identical. Original argv
is retained for explicit source retrieval, not repeated in normal execution records.

The broker serializes starts, mints an opaque execution ID, and atomically commits
intent before spawning. A key can only be removed together with completed history and atomic retirement
of its keyspace generation; its old pair can never launch again. The process PID/start ticks are saved once available. The pre-spawn recovery
ID survives the unavoidable spawn/record crash window.

A timeout or disconnect is **not** proof that start/input failed. Recover by IDs,
`list_sessions`, `open_session`, or the same previously persisted start key. Never
blindly retry a start without its original key, and never automatically resend stdin.
Read, wait, Resources, and checkpoint operations contain no command spawn path.
An intent without confirmed start/termination evidence returns `outcome_unknown`.
It does not automatically try to start again, even if the crash happened before spawn.
This is duplicate suppression with explicit uncertainty, not an exactly-once execution
claim. There is deliberately no stdin retry key in this change.

Stdin has a two-phase receipt. Before queueing, the broker sets both the record's
`input` receipt and a `prepared` event and requires a successful atomic durable commit.
Any write/fsync/rename/directory-fsync failure prevents queueing and returns
`stdin not sent` with `queued=false`; receipt durability is unknown, not asserted false.
After queueing, `accepted` means queue acceptance only. That receipt is also committed
before a success ACK. Failure of this second commit returns `delivery_unknown` with
`queued=true`, never success or permission to resend. The actor subsequently records
`written` or `delivery_unknown` independently of queue acceptance. Recovered `prepared`
and `accepted` receipts become `delivery_unknown` with unknown queue status: even a
persisted preparation cannot establish whether a crash happened before or after queueing.

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
| Execution records / retained start keys | 128 retained records; retire eligible completed history under pressure; refuse only if unfinished/pinned evidence occupies capacity |
| Reader checkpoints | 128; retire eligible completed executions if needed; reject when remaining readers are unfinished |
| Durable output history | 1 MiB / 1024 events globally, oldest events removed |
| Recovery snapshot | 4 MiB hard bound; new starts target at most 2 MiB metadata to leave space for logs and receipt/checkpoint growth; atomic replacement may require another 4 MiB |
| Source argv | Existing 64 KiB serialized command bound |
| Purpose / completion condition | 2048 UTF-8 bytes each |

The existing live process limit remains 64. New starts compact the oldest eligible
completed records when the record or metadata budget requires room. This is a retained
window, not a lifetime execution limit. Each compaction atomically rotates the
server-minted key generation with the record/key removal. A key miss in any previous
(or unknown) generation is rejected; no unbounded tombstone/key set is required.
Retained intents may still be replayed with their original generation, even after a
rotation. Original argv, logs, and completed checkpoints for a retired execution are
removed together. A read of retired evidence returns an explicit unavailable/retired
error, not an empty successful result. Epoch watermarks remain while any record in that
epoch survives, so a reader's valid global cursor does not become a false future cursor
when another execution is retired.

Unfinished `starting`/`running` and `outcome_unknown` records are never retired.
An incomplete reader checkpoint pins only its referenced execution. Explicit `work_id`,
`purpose`, or `completion_condition` also pins a declared phase before its first checkpoint;
confirmed process exit alone does not prove completion. A first checkpoint also
promotes a metadata-free start to durable declared work and preserves its purpose and
completion condition on the execution. Process events and reader moves cannot undo
this promotion; explicit completion is still required before retirement. After verifying the condition,
a reader can CAS `completed:true` on a checkpoint for a confirmed exited execution.
This persists completion evidence for that phase (the existing `work_completed` field),
without completing other phases of the same work. Retirement requires this evidence for
declared phases and no unfinished reader referencing that execution. Later incomplete
checkpoints do not erase the evidence: they pin their referenced phase until completed
or moved. Moving readers cannot release a phase that was never explicitly completed.
Thus a failed new phase does not pin completed history or prevent a repair start when
completed history can be retired. Work purpose and completion condition remain on
retained records and reader checkpoints, including when readers move between phases.
Running/unknown phases are never marked complete by another phase's checkpoint.
This is an explicit caller assertion, not an automatic
work-completion inference. A full window of genuinely unfinished/unknown work still
refuses new starts without discarding evidence. Operators must resolve pending work;
no state reset or automatic replay is a recovery path.

Legacy keys without a generation remain replayable while their intents are retained.
A new legacy-key miss is accepted only until the first compaction; thereafter it is
rejected permanently. Fresh keyed operations must use the advertised generation.
Existing tools-only clients that do not use keys can keep starting fresh commands;
they still must not blindly resend after a lost ACK. Existing version-1 snapshots
migrate to version 2 with their old intents intact. Loading v1 or older v2 snapshots
also promotes executions referenced by existing checkpoints and restores missing work
metadata from those checkpoints after checking their session/work binding. Loading
does not infer completion. No production state is migrated by
these local tests.

Storage failures disable further starts and checkpoint mutations for that broker
lifetime and expose `persistence_ok:false`. Reads remain evidence. Snapshot retirement
and generation change commit together before spawn: even a rename followed by a failed
directory fsync yields either the old retained key or an expired generation/unknown
intent after restart, never an admissible forgotten old key.

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
It stores the processed `detail_cursor`, purpose, completion condition, completion
assertion, and a revision.
An existing binding can move to the next execution in the same work through CAS.
At reader capacity, fully completed other work may be retired atomically with the CAS
and generation change; the mutation is therefore advertised as potentially destructive.
Within one execution the cursor cannot move backwards or cross epochs; global sequence
gaps caused by other executions remain valid. Successful read alone never writes it.

Two readers have independent acknowledgements. CAS conflict, lost checkpoint ACK, or
uncertain storage requires reading the current record and reconciling; it does not
justify re-executing a command or treating unprocessed detail as acknowledged.
The server validates position/revision, while the reader attests that it processed the
page. The caller must verify the completion condition before explicitly setting `completed:true`;
the server validates confirmed exit, not the semantic truth of that condition. Omission
preserves the completion flag for the same execution and defaults false for a new one.
A checkpoint never schedules more work.

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
