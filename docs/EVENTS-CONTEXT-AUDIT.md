# Events delivery and model-context audit — 2026-10-07

Reviewed source and live Git HEAD: `4d1d1d73ff5d5124087faf8e2ce393316b1bc6ac`. The server payload, runtime and subscription lifecycle remain unchanged. The receiving parent manages the automation prompt separately.

## Finding

No server contract violation explaining missing event bodies in the receiving chat has been established. Expanding the nested schema or duplicating output as a top-level text preview is **not a demonstrated requirement for event-driven execution**. Those changes are deferred. Valid webhook receipt, task wake-up, event data in model input, and chat forwarding are separate observations.

The [OpenAI Events guide](https://developers.openai.com/plugins/build/mcp-events) requires matching event names, payloads conforming to the advertised schema, stable IDs across retries, occurrence timestamps, and signed webhook delivery. It distinguishes webhook acceptance from asynchronous ChatGPT processing; task batching is a client setting. Its test procedure separately verifies delivery acceptance and expected data reaching ChatGPT. It does not specify the exact model-context projection or promise one model run per output chunk.

## Current contract and implementation

| Concern | Existing representation | Source |
| --- | --- | --- |
| Protocol envelope | `eventId`, `name`, `timestamp`, `data`, `cursor` | `rust/src/events.rs:536` |
| Execution identity and output ordering | `data.events[].execution_id`, `session_id`, `sequence`, `kind`, `timestamp` | `rust/src/broker.rs:65` |
| Output and termination | Nested record `data.stream` / `data.text`, or `data.exit_code`, according to kind | Broker journal; passed through unchanged |
| Recovery information | Envelope `cursor`; `data.catch_up_required`, `data.earliest_cursor`; bounded `read_execution` | Events sender and broker |
| Exact-body signing and retry identity | Standard Webhooks headers; persisted serialized body and ID; fresh attempt timestamp | `rust/src/events.rs:121`, `:536` |
| Delivery acknowledgement | Acknowledged cursor advances only on HTTP 2xx; pending body retained for transient retry | `rust/src/events.rs:566` |

The current payload schema requires an events array, boolean catch-up flag, and string earliest cursor. Its item schema is `{"type":"object"}`; it does not set nested `additionalProperties:false`. Therefore the existing execution IDs, sequences and record data are valid nested properties, not fields forbidden by the schema. [JSON Schema object applicator semantics](https://json-schema.org/draft/2020-12/json-schema-core#section-10.3.2.3) allow these properties when that restriction is absent. Schema validation is not an instruction to remove them.

The [MCP Events proposal](https://github.com/modelcontextprotocol/experimental-ext-triggers-events/blob/main/docs/design-sketch-proposal.md#eventoccurrence-schema) places deduplication identity in `eventId` and optional replay position in the envelope `cursor`. Neither copying that cursor into application data nor duplicating output text is required by that contract. Describing nested fields more fully may help discoverability, but its effect on ChatGPT context delivery is unverified.

## Evidence boundary

The operational report supplied to this audit says the current release delivered accepted webhooks and woke the parent chat. It also says ten paced outputs were forwarded using short `read_execution` polling; that is **not Events-only end-to-end evidence**. No raw webhook body paired with the receiving model input from that same event has been supplied. Source inspection establishes the serializer's fields, not the platform's received-body trace.

This audit confirmed the ARM64 project checkout and remote main both at the reviewed SHA, with no tracked changes before documentation edits. It did not repeat the prior command tests, refresh or remove the persistent subscription, restart the live service, or run a new streaming probe.

## Minimal receiving-side comparison

For one explicitly requested streaming check, use the existing subscription and one newly identified harmless execution producing two distinct stdout markers and one stderr marker at spaced intervals, with a later exit. Do not rerun the earlier test set.

1. Before starting, the receiving parent records the intended execution ID/start cursor and requests immediate forwarding for **this execution only**. Ordinary logs remain inputs to ongoing work rather than automatic chat messages. Automation prompt changes belong to the receiving parent.
2. Record what actually reaches the parent at each event wake-up **before any recovery tool call**: whether it includes the envelope or only application data, event ID/cursor if available, and nested execution ID, sequence, output/state/exit fields. Preserve timestamps and identify any markers obtained from `read_execution` separately.
3. Compare the same event with the sender's exact serialized-body evidence if available. Existing pending state holds that body only until acknowledgement. The current server has no retained per-event post-ack transport trace; an advanced ACK cursor alone cannot prove which fields the host supplied to the model. Do not dump the subscription store, signing keys, callback URL, or credential files.
4. If body evidence exists on the sender and fields disappear before the model input, investigate host callback ingestion, batching and context projection. If sender evidence lacks those fields or violates its schema, fix that demonstrated server defect. Without both sides, label the boundary unresolved.
5. Deduplicate protocol events by event ID and journal records by broker epoch plus sequence. Keep the supplied opaque cursor for replay/recovery. Broker sequence is global: filtered executions may legitimately skip numbers, so a numeric jump alone is not proof of loss. Recover from the last received cursor only after identified loss, reconnect, a catch-up flag, or a wake-up missing the expected body; do not turn ordinary delivery into a polling loop.

If output cannot be handled until execution completion, Events-only streaming has not been demonstrated, even when all markers eventually appear. Delivery acceptance and a task wake-up do not establish that stronger result.
