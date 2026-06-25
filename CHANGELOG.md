# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases may introduce breaking changes freely as the DSL and
runtime shape converge. After 1.0, changes will follow semver strictly.

## [Unreleased] - 0.7.8

### Fixed — `output otlp_http` / `output otlp_grpc`: retain drained batch on mid-stream flush failure

When `flush()` returned `Err` mid-stream, both OTLP transports dropped the events they had just drained from the in-memory buffer — the per-Event ResourceLogs bytes were popped to build the request, the request failed, and the bytes were discarded with only a `tracing::warn!`. The retry budget did not apply (the queue layer had already counted the events as delivered when `write()` returned `Ok` into the batch buffer), so the next flush started from a fresh buffer and the drained payload was silently lost. The drained batch is now restored to the in-memory buffer on `Err`; `events_failed` is **not** bumped (the events have not actually been rejected by the peer, only deferred), and the next flush picks them up alongside whatever has accumulated since. Aligns the mid-stream failure shape with the existing shutdown-flush behaviour (PR-P): events stay in the buffer until either a successful flush or shutdown drains them — never dropped silently.


### Fixed — `output syslog_tcp` / `output syslog_udp`: peer cooldown anchored on failure-completion time

The peer-rotation cooldown timestamp on both syslog outputs was being captured *before* the connect / send attempt, the same shape `output http` already fixed in 0.7.8 (commit `e0484e9`) and `output otlp_grpc` / `output otlp_http` picked up via the sibling fix below. A peer that timed out mid-send recorded a cooldown that was already most of the `PEER_COOLDOWN` window in the past, so the immediately-following datagram / connection attempt reselected the same bad peer instead of rotating away. The cooldown timestamp now derives from a fresh `Instant::now()` captured on the failure branch after the failing call returns, matching the rest of the rotation-aware outputs and giving the cooldown window the wall-clock distance it needs to actually shift load to a healthy peer.


### Fixed — `output http`: stuck batch after a failed flush no longer waits for the next `write()`

When `flush()` returned `Err`, the batch was placed back into the in-memory buffer but the flush timer was not re-armed. The stuck batch then sat in the buffer until the next `write()` arrived — which on a quiet pipeline might be never — while the queue layer had already counted the events as failed (Rendered payloads do not retry). The operator saw `events_failed += 1` yet the data still lived in the HTTP buffer with no schedule to drain it. The flush timer is now re-armed on the `Err` branch so `batch_timeout` drives the retry, restoring the "no event silently parked in the HTTP buffer" invariant. Regression test covers the `should_flush` failure path.


### Fixed — `limpid-prometheus`: exporter scrape against a wedged daemon is bounded at 5 s

The Prometheus exporter previously used `std::os::unix::net::UnixStream` + blocking `BufRead::lines()` from inside async hyper handlers. A wedged limpid daemon — accepting the control-socket connection but never writing a reply — pinned a tokio worker thread until the daemon answered, with no upper bound; slow / stuck scrapes silently starved the exporter's runtime and Prometheus scrapes piled up on the broken peer. The control-socket query is now `tokio::net::UnixStream` + `AsyncBufReadExt` and the entire connect+write+read sequence is wrapped in `tokio::time::timeout(QUERY_TIMEOUT = 5 s, …)`. A scrape hitting the cap returns an error body and the next scrape gets a fresh attempt instead of waiting behind the old one. 5 s is well above local control-socket latency (typical < 1 ms) and well below Prometheus' usual `scrape_timeout` (10 s).


### Fixed — `input tail`: saved offset zero now resumes from 0, and send failure rewinds the cursor

Two silent data-loss bugs on the cursor-persistence side, fixed together. (1) `load_position().unwrap_or(0)` plus a follow-up `if offset == 0` collapsed "no state file" and "saved `Some(0)`" into the same path, sending the cursor to EOF and skipping every line appended between the save and the next start — the typical recovery shape after rotate/truncate. The path now keeps the `Option<u64>`: `Some(n)` resumes from `n` (including 0), `None` falls back to EOF. (2) `read_new_lines` advanced `current_offset` past each line before sending it downstream. If `tx.send()` failed (consumer gone) the loop broke out and `run()` persisted the already-advanced offset, silently dropping the un-sent line. The send-failure path now mirrors the incomplete-line rewind so the line is retried on the next poll.


### Fixed — `input journal`: blocking reader exits promptly on shutdown while idle

The journal input runs its libsystemd-backed reader inside `tokio::task::spawn_blocking`, and on shutdown the orchestrator called `journal_handle.abort()`. tokio's abort cannot cancel a `spawn_blocking` task that has already started executing, so the reader's only escape route was the next `tx.blocking_send()` returning `Err` — which requires a fresh journal entry to arrive. On a quiet host that may not happen for a long time, leaving the blocking thread (and its journald file handle) parked indefinitely past daemon shutdown. Shutdown is now signalled explicitly via an `Arc<AtomicBool>` the reader polls between iterations, and the per-poll sleep is replaced with `interruptible_sleep` that naps in 100 ms quanta re-checking the same flag. Shutdown latency is bounded by one quantum regardless of `poll_interval`.


### Fixed — `output http` / `otlp_http` / `otlp_grpc`: in-memory batch is flushed on shutdown

The three batched sinks return `Ok` from `write()` once the event lands in their in-memory buffer (so the memory queue counts it as delivered), and the flush either happens when `batch_size` is hit or when the per-output timer fires. On daemon shutdown those sinks' `Drop` impl aborted the timer and the process exited with the buffer contents still resident; the existing log line claiming events "will be re-delivered from queue" is not true for the memory queue. `Drop` cannot fix this because it is synchronous and the sink I/O is async. `Output` gains an async `shutdown()` hook with a default no-op, overridden on the three batched sinks to abort the timer and run one final flush. `run_queue_consumer` calls it once the consume loop exits, so both the shutdown-signal and queue-closed break paths fall through the same shutdown call.


### Fixed — `runtime`: queue-enqueue and pipeline eval-error failures now reach the DLQ

Two High pipeline-side error-path gaps closed together. (1) `run_pipeline_with_outputs` discarded the bool returned by `QueueSender::send`. On enqueue failure (memory-queue receiver dropped, disk serialise/write error, Rendered-on-Disk routing bug) the pipeline counted the event as `events_finished` even though it had reached neither the queue nor the secondary nor the error log — the event was effectively deleted in silence. The bool is now captured, failed output names collected, and termination overridden to `Errored` so the existing Errored-arm DLQ machinery catches it. The per-output `events_failed` metric is also bumped so operators see the failure on each output's dashboard regardless of the pipeline-level routing decision. (2) `process_event` matched `Err(e)` returned from `run_pipeline` with a log-only branch — no `events_errored` bump, no DLQ entry — breaking the docs' promise that runtime errors go to `events_errored` and the error log. Both paths now construct an `ErroredEventContext` and route through a shared `write_errored_to_dlq` helper.


### Fixed — `queue`: disk cursor commits on consumer ack, not on `recv()`

High-severity audit finding. The disk queue saved its read cursor inside `recv()` immediately after each event was returned, so from the queue's POV the event was consumed before the queue consumer even handed it off downstream. A crash between `recv` and the output write lost the event: on restart the persisted cursor sat past the un-shipped record and it was never replayed — defeating the "retry / restart re-delivers" contract the disk queue exists for. The queue now follows the standard durable-queue contract (Kafka/RabbitMQ shape): `recv()` only advances an in-memory cursor, and a new `ack()` hook persists progress + reclaims consumed segments. The queue consumer calls `ack()` after every event's disposition is decided — delivered, routed to secondary, or retries exhausted — so the on-disk cursor only moves once the event has reached a terminal state. Memory-queue's `ack()` is a no-op. This shifts the disk queue from at-most-once to at-least-once; downstream sinks that can't tolerate duplicates need idempotent ingestion.


### Fixed — `queue`: disk cursor uses per-event ack position (regression fix)

The `DiskQueueReceiver::ack()` method introduced in this cycle saved the
receiver's current `read_seq` / `read_offset` instead of the position of the
event whose handle was being acked. With batched outputs holding multiple
`(Event, QueueAckHandle)` pairs in flight, a single event ack advanced the
cursor past **all** buffered events; a crash before the remaining events
flushed silently lost them — defeating the at-least-once guarantee the same
cycle's "disk cursor commits on consumer ack" entry advertised.
`QueueAckHandle` now carries its position, the receiver tracks an in-flight
position queue, and the persisted cursor only advances through the contiguous
acked prefix from the front. Memory queue is unaffected (no persistent cursor).


### Fixed — `queue`: retry-exhausted and unrecoverable payloads now flow to `error_log`

Output retry exhausted with no usable secondary path previously dropped the payload silently — the consumer ack'd, the event left the disk queue's replay window, and only a `tracing::warn!` plus an `events_failed` increment remained. Same shape when a secondary was configured but its enqueue also failed: the original event was already consumed and the failure was log-only. Retry exhaustion (and failed-secondary fall-through) now write the payload as a JSONL record to the configured `control { error_log "..." }` — the same DLQ that pipeline / process eval errors flow into. Output-failed records ride the same writer with `pipeline=""` and `process="(output <name>)"` as the discriminator, so operators reading the DLQ stream see them alongside pipeline failures. When no `error_log` is configured the previous warn-only fallback is preserved (no regression).


### Fixed — `output http` / `otlp_http` / `otlp_grpc`: shutdown-flush failures drain to `error_log`

The batched outputs' final `shutdown()` flush retained the in-memory buffer on failure for "retry", but there is no next retry tick at process exit — so the retained buffer was equivalent to dropping the events. The shutdown trait signature now takes an optional `&Arc<ErrorLogWriter>`; when the final flush fails the helper walks the remaining buffer items and persists each as an `ErroredEventContext` with `process="(output <name> shutdown)"` (distinct from the retry-exhausted discriminator so operators can tell mid-stream from at-shutdown failures). When `error_log` is not configured the shutdown error propagates unchanged (0.7.7 parity); when the error_log writer itself fails the helper swallows the secondary error to avoid recursion and the original shutdown error still surfaces.


### Behavior changes (non-breaking)

#### Outputs: `retry { ... }` and `secondary <name>` are now accepted on every output type

The runtime has always honoured `retry { ... }` and `secondary <name>` on every output (the queue layer reads them uniformly via `RetryConfig::from_output_properties`), but the property schema only declared them on `output otlp_grpc` and `output otlp_http`. Writing either property on `kafka`, `file`, `http`, `stdout`, `syslog_tcp`, `syslog_udp`, or `unix_socket` failed `--check` with "unknown property", even though the documented `outputs/README.md` examples implied they were universally available. The schema was the gap, not the runtime and not the docs. `RETRY_PROPERTY_SPEC` and `SECONDARY_PROPERTY_SPEC` are now lifted into `queue/mod.rs` and spliced into every output's schema; the prior OTLP-local `RETRY_BLOCK_PROPERTIES` (with `max_attempts` / `initial_wait` / `max_wait` / `backoff`) is preserved unchanged for the existing call sites.


### Upgrading — additional configs that now fail-fast (0.7.8 cycle, second batch)

Four further config shapes are rejected at parse / startup time in 0.7.8. Each is individually described in the `Fixed —` entries above; this list is a single place for operators to scan before upgrading.

- **`secondary <name>` referencing an unknown output, or forming a cycle (direct or indirect).** A typo (`secondary foo_typo` with no matching output) used to silently disconnect the safety net; a self-reference (`secondary <own_name>`) or a multi-hop cycle (`A -> B -> A`, `A -> B -> C -> A`, …) used to loop poison events forever on retry exhaustion and, on disk-backed queues, grow the queue file unboundedly. Both shapes are now rejected by `CompiledConfig::validate`, which `limpid --check` and runtime startup both call — so CI gates the config before the daemon would refuse to boot. Unknown-target errors list the configured output names; cycle errors report the full cycle path so the operator can see which edge to remove.
- **`switch` arms with `default` not last, or with more than one `default`.** The runtime walks arms in source order and `default` matches everything, so any arm after a `default` is unreachable and multiple defaults are ambiguous (only the first runs). Pre-0.7.8 `--check` was silent on this shape — configs that meant "case 6 → tcp, otherwise null" but accidentally put `default` first sent every event to the default branch with no diagnostic. Both shapes now fail `--check` as `DiagKind::Dataflow` errors. Remediation: move `default` to the last arm and remove duplicates.
- **Recovery-dependent outputs without a configured `control { error_log "..." }`** now emit a `--check` warning. The retry-exhaustion and shutdown-flush recovery paths added in this cycle only activate when `error_log` is configured; an operator who configures `retry { ... }`, `secondary <name>`, or any batched output (`http` / `otlp_http` / `otlp_grpc`) but forgets `error_log` gets the same 0.7.7 silent-drop behaviour. The new warning fires once per affected configuration. Under plain `--check` the warning is informational; under `--check --strict-warnings` (and `--ultra-strict`) it is promoted to a hard fail per the existing strict-warnings ladder.
- **`secondary "name"` (quoted string) is rejected.** The newly-broadened `SECONDARY_PROPERTY_SPEC` initially accepted any string-shaped scalar (string literal, template, bare ident), but the runtime reads the secondary via `props::get_ident` — bare-ident only. A quoted `secondary "fallback"` therefore passed `--check` while the runtime silently dropped the secondary on the floor (and the `recovery_readiness` warning stayed silent because it walks the same `get_ident`). A new `PropertyValueKind::Ident` variant gates the spec and only `ExprKind::Ident` with a single segment is accepted; string literals, templates, and dotted paths fail with TypeMismatch ("a bare identifier (output name, not a quoted string)"). Remediation: drop the quotes — `secondary fallback`.


### Internal — Unified switch / if-chain dispatch across pipeline and process contexts

The runtime executed switch and if-chain dispatch twice — once for pipeline context (`pipeline.rs` PipelineStatement::Switch / `exec_pipeline_if`) and once for process context (`dsl/exec.rs` ProcessStatement::Switch / `exec_if_chain_process`). Both walked arms / branches in source order with first-match semantics; the divergence was entirely in the surrounding execution context. The dispatch algorithm is now factored into two pure helpers in `dsl/eval.rs` (`select_switch_arm` and `select_if_branch`) that take an `eval_*` closure capturing the caller's context-specific state and return the matched body as a slice for the caller to execute. `exec_pipeline_if` and `exec_if_chain_process` are eliminated outright; the per-context dispatch shrinks to a 4-line match. The free `is_truthy` wrapper in `eval.rs` is also removed; the canonical `Value::is_truthy` impl stands alone. No user-visible behaviour change.


### Internal — Queue I/O boundary functions return typed outcomes instead of `bool`

`QueueSender::send`, `DiskQueueSender::send`, and `write_with_retry` previously returned `bool`, where `false` could mean any of several distinct things (queue closed, disk write failed, serialization failed, retry exhausted, secondary handed-off) and callers had no type-level signal that the value mattered. Two new outcome types in `queue/outcome.rs` replace the booleans: `QueueSendError` (an enum of the failure modes) and `WriteDisposition` (`Delivered` / `RoutedToSecondary` / `Dropped`, later extended with `DroppedToRecovery` for the `error_log` routing path). Both are `#[non_exhaustive]` and `WriteDisposition` is `#[must_use]`, so call sites are forced to handle the disposition and future variants will surface as compiler errors. No operator-visible behaviour change at the time of this refactor — it is the type-level foundation the subsequent recovery-routing fixes build on.


### Fixed — `output otlp_grpc` / `output otlp_http`: route `partial_success.rejected_log_records` to `events_failed`

When the OTLP receiver returned 2xx-equivalent with `partial_success.rejected_log_records > 0`, both transports counted the entire batch as `events_written`, hiding server-side data loss from operator dashboards. `otlp_grpc` parsed the response and logged a warning but did not split the metric; `otlp_http` did not parse the response body at all. The OTLP transport-success path now splits the batch's events between `events_written` (accepted) and `events_failed` (rejected) using the receiver's `partial_success.rejected_log_records`. `otlp_http` learned to decode the response body in both protobuf and JSON forms — peers returning empty bodies or undecodable bodies are still treated as fully accepted (the lenient default). Selective re-send of *only* the rejected records remains queued for a later release, as documented in the existing `send_once` doc comments; this change is purely metrics accuracy.


### Fixed — `output otlp_grpc` / `output otlp_http`: stop silently dropping distinct `schema_url`s when merging by Resource / Scope

`merge_by_resource` (and the inner Scope-level pass in `merge_by_scope`) keyed merges only on Resource (or Resource + InstrumentationScope) equality. Two entries sharing a Resource but declaring *different non-empty* `schema_url`s — semantically: "the same resource described under two different schemas" — collapsed into a single bucket and the second `schema_url` was dropped on the floor. Per OTLP semantics they should remain distinct. The merge key now also requires `schema_url` compatibility (equal, or at least one side empty), so different non-empty `schema_url`s keep their own bucket. The existing "promote empty acc → take incoming schema_url" behaviour is preserved (and now regression-guarded).


### Upgrading — configs that now fail-fast (action required if matched)

0.7.8 turns three previously-tolerated misconfigurations into hard parse-time errors. If a 0.7.7 config matches any of the patterns below, the daemon will refuse to start on 0.7.8 and limpidctl check will reject it:

- **`output kafka` with `mechanism plain` and no `tls { ... }` block.** SASL/PLAIN sends credentials in clear text, so 0.7.8 requires a TLS transport. Remediation: add a `tls { ... }` block (CA only is fine for a server-cert-validated peer) or switch to `mechanism scram_sha_256` / `scram_sha_512`, which use challenge-response and never put the password on the wire.
- **`output otlp_http` with a `tls { ... }` block on an `http://` endpoint** (and now `output otlp_grpc` too, added in the 0.7.8 sibling-regression follow-up). reqwest and tonic only engage TLS when the URI scheme is `https`, so the previous behaviour silently dropped the TLS block and shipped in clear text. Remediation: change the endpoint to `https://...`, or drop the `tls { ... }` block if plaintext is intended.
- **`output http` with a `method` other than POST / PUT / an extension token reqwest accepts.** 0.7.7 silently downgraded unknown methods to POST; 0.7.8 fails fast at parse time. Remediation: spell the method correctly. The set of accepted methods matches reqwest's `Method::from_bytes` — uppercase, ASCII.

No CHANGELOG entry intentionally hides any of these; they are individually called out in the `Fixed —` entries below. This summary just gives operators upgrading from 0.7.7 a single place to scan before the upgrade.


### Internal — End-to-end timeout-firing tests for the 0.7.8 export and TLS-handshake timeouts

The three timeout constants introduced in 0.7.8 — `GRPC_REQUEST_TIMEOUT` / `HTTP_REQUEST_TIMEOUT` (30 s, on the OTLP sinks) and `TLS_HANDSHAKE_TIMEOUT` (10 s, on `input syslog_tcp`) — previously had bound-check assertions only. A regression that removed the `tokio::time::timeout(…)` wrap, or pointed it at a much larger duration, would not have been caught by a constant-value check. Three new paused-time tests (`export_timeout_fires_against_stalled_peer` in each of `output/otlp/grpc.rs` and `output/otlp/http.rs`, plus `tls_handshake_timeout_fires_against_stalled_client` in `input/syslog_tcp.rs`) exercise the actual firing path against a stalled TCP peer / client. Each uses `tokio::time::advance` past the documented timeout and asserts the call surfaces a timeout-flavoured error rather than hanging. `tokio`'s `test-util` feature is added to `[dev-dependencies]` to enable virtual time control. No production code change.


### Fixed — `output http` / `otlp_http` render a placeholder when error bodies are gzip/brotli/deflate encoded

limpid's `reqwest` build excludes the `gzip` / `brotli` / `deflate` decompression features, so when a peer (or upstream proxy) returns an error response with `Content-Encoding: gzip` the still-compressed bytes were running through `from_utf8_lossy` and ending up as replacement-char soup in the daemon log. The shared `error_snippet` helper in `modules/output/http_util.rs` now inspects `Content-Encoding` and substitutes `<gzip-encoded body, N bytes>` (or whatever the advertised encoding is) when it's not `identity`. The byte count is retained so an operator can still see the peer is returning *something*. `identity`, missing header, and the existing 4 KiB cap path all keep their previous behaviour.


### Fixed — `output syslog_udp` walks every resolved address on connect, restoring DNS-level failover

The 0.7.8 family-aware bind rewrite kept v6-only destinations working but regressed DNS failover: `lookup_host(host:port).next()` committed to the first resolved `SocketAddr` and gave up if that one didn't connect. Pre-0.7.8 `socket.connect(host:port)` walked the whole resolution list internally and succeeded on the first reachable address — common during a partial v6 outage or a stale AAAA record on a dual-stack host. The connect path now iterates every resolved `SocketAddr`, binding a fresh ephemeral socket of the matching family per attempt and breaking on first success. On exhaustion the most recent error is returned with both the original hostname and the specific address that failed, so an operator can see which records were tried.


### Fixed — `output kafka` reports PLAIN-without-TLS before reading the password file

A misconfiguration with both a broken `password_file` path and `mechanism plain` without a `tls { ... }` block surfaced the file-read error first, masking the more important credentials-on-the-wire problem. The operator would fix the file path, get the daemon to start, and only then discover their PLAIN config was unsafe. `kafka.rs` now does a cheap pre-check on the mechanism ident before `parse_sasl_block` touches the filesystem, so the PLAIN-without-TLS diagnostic always fires first. The post-parse `require_tls_for_plain` guard stays as a belt-and-braces check, and the new pre-check explicitly avoids leaking the password-file path into the error wording. Two new tests cover both branches.


### Internal — `limpidctl check`: OneOf schema edge cases documented + multi-block guard

Two follow-ups on the OneOf branch-picking logic that landed in 0.7.8. (1) `check_one_of` now documents — with a regression test — the deliberate fallback to `OneOfMismatch` when 0 or 2+ variants structurally match. Two-scalar-variant OneOf given a wrong-type literal (e.g. `OneOf[String, Int]` with a Bool) keeps the "expected String | Int, got Bool" wording, which is more useful than picking one variant's TypeMismatch and hiding that the other shape was also allowed. (2) `inner_block_schema_of` in `check/outputs.rs` previously returned the first block-shaped OneOf variant via `find_map`. Today only `OneOf[Block(TLS_CLIENT_BLOCK_PROPERTIES), String]` exists, so "first block wins" is unambiguous — but a future `OneOf[Block(A), Block(B)]` (e.g. inline tls vs inline mTLS configs) would silently validate against the wrong schema. The function now returns `None` when more than one block-shaped variant exists, falling back to expression-level checks until a per-OneOf resolution rule is encoded explicitly. No user-visible behaviour change today.


### Fixed — `sum()` decides accumulator type from the whole array, not the first Float

The 0.7.8 i64-overflow fix tripped on `[i64::MAX, 1, 0.5]`: the second integer overflowed the i64 accumulator before the third element (a Float) had a chance to promote the result. The eventual return type was clearly going to be `Float`, but the operator got a hard error instead of the float total they were summing toward. `sum()` now pre-scans the array for any Float and picks the accumulator type up front — Int-only arrays still use a checked `i64` accumulator (overflow surfaces a typed error with a remediation hint suggesting `* 1.0` promotion); any-Float arrays use a single `f64` accumulator and follow IEEE 754 semantics (overflow saturates to ±Infinity, NaN propagates). The expression-functions doc note is corrected at the same time — the prior `map(...) { |x| x as f64 }` suggestion referenced an `as` cast operator the limpid DSL does not implement; the working idiom is `map(...) { |x| x * 1.0 }`. Five new tests cover the boundary (mixed int+float past i64::MAX, float-only, float overflow → +Inf, NaN propagation, and the remediation hint in the overflow error).


### Fixed — `output otlp_http` now warns loudly when `verify false` is paired with an https endpoint

`output http` already emits a one-line, greppable `tracing::warn!` when `verify false` is paired with an https URL, so operators can audit the daemon log for MITM-vulnerable peers. `output otlp_http` exposes the identical `verify` knob but had no such warning — `verify false` toggled `danger_accept_invalid_certs(true)` silently, so the same security-relevant misconfiguration was visible in one output and invisible in the other. The warn now fires once per https peer at startup with the same wording as the `output http` message.


### Fixed — `output otlp_grpc` rejects `tls { ... }` on plaintext `http://` endpoints

Same trap `output otlp_http` already closes for itself in 0.7.8: tonic only engages the TLS layer when the URI scheme is `https`, so a `peer { endpoint "http://otel:4317"; tls { ca ...; cert ...; key ... } }` configuration silently dropped the entire TLS block and shipped gRPC in clear text — exactly the misconfiguration an operator who took the trouble to write a `tls` block was trying to avoid. The mismatch is now rejected at parse time with the same error wording as the `otlp_http` guard: switch the endpoint to `https://` or drop the `tls` block.


### Fixed — `output otlp_grpc` and `output otlp_http` now bound peer cooldown from failure time, not request start

The peer-rotation cooldown timer was being measured from a pre-request `Instant::now()`, the same bug `output http` already fixed in 0.7.8 (commit `e0484e9`) and which was not propagated to the OTLP sinks. With the newly-introduced 30 s export timeout and a 5 s `PEER_COOLDOWN`, a peer that timed out wrote a cooldown that was already 25 s in the past, so the immediately-following batch reselected the same bad peer instead of rotating away. The cooldown timestamp now derives from a fresh `Instant::now()` captured on the failure branch in both `otlp/grpc.rs` and `otlp/http.rs`, matching the `output http` fix and giving the rotation budget the wall-clock distance it needs to actually shift load to a healthy peer.


### Fixed — `output otlp_http` no longer buffers unbounded error bodies into memory

The peer-failure diagnostic path used `resp.text().await` and then trimmed the resulting `String` to 500 chars. Because `text()` buffers the entire response body before returning, a peer (or upstream proxy) emitting a multi-MB error body forced the daemon to allocate / decode the full payload on every failure — an availability footgun the matching fix in `output http` already closed. `output otlp_http` now reads via the shared `read_body_capped` helper with the same 4 KiB cap, so the cost of a failing peer is bounded regardless of how chatty its error responses are.


### Internal — `read_body_capped` extracted to a shared helper

`output http` and the soon-to-be-aligned `output otlp_http` both need to bound how many bytes of an error response body they read into memory, so the helper moved from `modules/output/http.rs` to a new `modules/output/http_util.rs` module. No behaviour change for `output http`. The lingering misleading comment that claimed the connection "returns to the pool" after a mid-chunk break is also corrected: reqwest/hyper closes the underlying TCP connection when the `Response` is dropped without reaching EOF, and that's an accepted trade-off (bounded memory matters more on a failing peer than reusing its connection).


### Fixed — Docs: fenced code blocks now tagged for markdownlint MD040 compliance

`docs/src/{dsl-syntax,functions/expression-functions,processing/user-defined,inputs/syslog-tcp,outputs/syslog-udp}.md` had unannotated fenced code blocks. mdbook-style consumers tolerate this, but markdownlint MD040 flags them and standard syntax-highlighting falls back to "no language". All 93 bare fences across these 5 files are now tagged `limpid` (the contents are uniformly limpid DSL — `def input/output/process { … }`, `workspace.x = …`, expression-function call sites). The accompanying `tls.rs` doc comment on `TLS_CLIENT_BLOCK_PROPERTIES` is also corrected: it claimed "empty `tls {}` block is rejected by callers", but the actual contract is module-specific (`output otlp_http` rejects on plaintext endpoints; other callers accept empty blocks as "use system CA roots"). Doc-only — no code path touched.


### Fixed — `sum()` now reports i64 overflow as a typed error

The integer accumulator used unchecked `+=` and depended on the
build profile for overflow behaviour: debug builds panicked,
release builds wrapped silently and produced bogus (often
negative) totals for large arrays. The accumulator now uses
`checked_add` and surfaces a typed error
`sum() overflowed i64 (accumulator …, element …)` regardless of
build mode, catching the bug in tests / `--check` instead of
production. Nine new unit tests cover the function (no inline
tests existed before): integer / mixed-numeric / empty-array
happy paths, type-error rejections (non-array input, null
input, non-numeric element), and the overflow boundaries at
`i64::MAX` + 1 and `i64::MIN` − 1.


### Fixed — `limpidctl check`: nested-block expression diagnostics + OneOf branch-specific errors

Two diagnostic-quality fixes from the PR #9 (release 0.7.4) review:

- **Expression-level diagnostics inside nested output blocks no longer
  silenced.** A typo like `peer { host "${upperr(workspace.msg)}" }`
  used to skip `expr_types::check_types` for `host` — the analyzer
  inherited the parent block's `schema_owned=true` flag through every
  recursion level and silenced every inner key, masking unknown
  functions, type mismatches, and similar expression errors inside
  any schema-declared nested block. The skip is now narrowed to the
  only case it actually targets — a bare top-level `ExprKind::Ident`
  value like `framing non_transparent` (= an enum-shaped value the
  schema validator owns) — so template interpolations inside nested
  output properties get checked again.
- **`OneOf` schema mismatches now surface the specific inner error
  when exactly one variant matched structurally.** Previously, when
  no variant matched cleanly, every failure collapsed to
  `OneOfMismatch` ("expected Block | Ident, got Block") — actively
  misleading when the user wrote the right outer shape and the real
  problem was one missing inner key. If exactly one variant matches
  the outer shape (no `ExpectedBlock` / `ExpectedValue` failure),
  the analyzer now surfaces that variant's specific inner error
  (e.g. `MissingRequired` for the missing `cert`). When zero or
  multiple variants structurally match, the generic `OneOfMismatch`
  still fires so the operator sees the full variant list.


### Fixed — `output syslog_tcp` / `output syslog_udp`: IPv6 + parse-path correctness

Three fixes from the PR #9 (release 0.7.4) review that surfaced once
this PR audit ran end-to-end on the current codebase:

- **`Peer::address` now brackets IPv6 literals.** A peer configured
  with `host "::1"` previously produced the address string `::1:514`,
  which Rust's `SocketAddr` parser rejects (it reads the trailing
  `:514` as part of the address). Both TCP `TcpStream::connect` and
  UDP `UdpSocket::connect` hit this. The formatted address now reads
  `[::1]:514`; IPv4 and hostnames are left unbracketed; an already-
  bracketed literal is preserved.
- **`output syslog_tcp` / `output syslog_udp` reject `peer` + `peers`
  in `from_properties` too.** The schema-validating `Module::build`
  path already caught this, but `from_properties` (called directly
  from snippet expansion and inline test fixtures) silently took the
  first `peer` block and discarded the `peers` block. The exclusivity
  contract is now enforced on every entry point.
- **`output syslog_udp` no longer forces an IPv4 ephemeral socket.**
  The previous hard `UdpSocket::bind("0.0.0.0:0")` meant any peer
  that resolved only to AAAA failed before the first datagram left.
  The output now resolves the peer first, picks `0.0.0.0:0` or
  `[::]:0` to match the resolved address family, then connects.


### Fixed — `input syslog_tcp`: TLS handshakes are now bounded at 10 s

A client that opened TCP but never completed the TLS handshake would
otherwise pin a task on `acceptor.accept().await` forever and consume
one of the `max_connections` slots. With enough stalled handshakes an
attacker (or a misbehaving client) could exhaust the slot pool and
deny service to legitimate peers. Handshakes now have a hard 10 s
ceiling; on timeout the connection is dropped with a `WARN` log
naming the peer address and the timeout duration.

### Fixed — `output http`: four correctness fixes from the 0.7.6 review

- **`verify false` no longer drops the client identity.** A `tls { cert
  key }` block on a peer used to be discarded entirely when `verify
  false` was set on the output, so mTLS silently broke whenever the
  operator disabled server-cert validation. The client identity is now
  preserved regardless of `verify`; only the `tls.ca` portion is
  ignored (with a warning) under `verify false`.
- **Peer cooldown now measured from the failure time.** With the new
  30 s per-request timeout and the 5 s peer-cooldown window, capturing
  `now` *before* the request meant a timed-out failure could record an
  already-expired cooldown and immediately reselect the bad peer.
  `Instant::now()` is now read after the call returns.
- **Method honored end-to-end.** Methods other than `POST` and `PUT`
  used to silently degrade to `POST`. The configured method is now
  parsed into `reqwest::Method` at config-load time (invalid verbs
  fail fast with a clear error) and sent verbatim via
  `client.request(method, url)` — `PATCH`, `DELETE`, `MKCOL`,
  RFC-compliant extension tokens all reach the peer as intended.
- **Error response body capped at 4 KiB.** A malicious or
  misconfigured peer used to be able to return an unbounded error
  body, which `response.text().await` would buffer in full before the
  caller trimmed it. The new `read_body_capped` helper stops reading
  at 4 KiB via `Response::chunk()` so the failure diagnostic stays
  bounded regardless of peer behaviour.

### Fixed — `output otlp_grpc` / `output otlp_http` / `output http`: Owned events no longer get silently merged into a batch

Disk-queue replay and control-socket inject events (`SinkInput::Owned`)
need a per-event ship verdict from the output module — `Ok` ⇒ drop from
the queue, `Err` ⇒ retry / disk-replay / secondary. The batched outputs
previously routed Owned events through the same buffer as the memory
hot path and returned `Ok` after only enqueueing the event, so the
caller never saw a per-event verdict. If the eventual flush failed the
buffered events were silently lost (the queue had already dropped
them).

The three batched outputs now override `write_owned` to ship a single
event inline, bypassing the batch, so the caller's queue retry / disk
replay semantics work as designed. The memory hot path (Rendered)
continues to batch as before.

### Fixed — `output otlp_grpc`: per-export 30s timeout

`client.export(request)` is now wrapped in `tokio::time::timeout(30s)`.
A collector that accepted the connection but never returned a HEADERS
frame would previously hold the flush future open indefinitely, blocking
rotation and starving retry. Matches the existing per-call timeouts
used elsewhere (syslog input, etc.).

### Fixed — `output otlp_http`: per-export 30s timeout + reject `tls { ... }` on plaintext endpoints

Two related corrections:

- The reqwest client now carries a 30s `timeout(...)` so a peer that
  accepts the connection but never replies counts as a failure and
  yields to the next peer in the rotation. Without this, a stalled
  collector blocked flush indefinitely.
- A `tls { ... }` block paired with an `http://` endpoint is rejected
  at config-load time. reqwest only negotiates TLS on `https://` URLs,
  so the previous behaviour silently shipped in clear text while
  pretending the tls block was active.

### Fixed — `output kafka`: reject `mechanism plain` without a `tls { ... }` block

SASL/PLAIN puts the username and password in clear text on the wire —
the only safe transport for that mechanism is TLS. Previously
`mechanism plain` paired with an absent `tls` block selected
librdkafka's `sasl_plaintext`, sending credentials to the broker in
clear text. limpid now refuses this combination at config-load time
and the daemon will not start until either a `tls { ... }` block is
added or the mechanism is switched to `scram_sha_256` / `scram_sha_512`
(SCRAM uses challenge-response and never puts the password on the
wire).

### Fixed — `output kafka`: SASL `password_file` handles CRLF / bare CR

Trailing-newline stripping now matches `\r\n` and bare `\r` in addition
to bare `\n`, so password files written on Windows hosts (or with an
editor that defaults to CRLF) authenticate correctly. Previously a
CRLF-terminated file left a `\r` on the password and produced a
`bad credentials`–shaped failure that looked like an operator typo.

## [0.7.7] - 2026-06-22

### Fixed — `cef.parse` now emits the raw extension blob as `ext`

`cef.parse` previously split the CEF Extension section into individual
`key=value` siblings of the header keys (`src` / `dst` / `act` / …) and
discarded the raw blob. There was no way to recover the original
extension string — needed for passthrough / re-emission, debugging the
splitter, and dialect-specific extension content the splitter doesn't
decode (escape sequences, custom separators).

The function now emits **both** forms: the split per-key form (the
documented authoring surface, unchanged) **and** the raw blob as
`workspace.cef.ext` (the new field). The raw form is omitted when the
Extension section is empty, mirroring `syslog.parse`'s treatment of
empty `msg`. `cef.parse` also gained the unit-test coverage that was
missing before — eight tests pin the header parse, extension split,
raw-`ext` emission, empty-extension omission, non-numeric severity
fallback, value-with-spaces splitter behaviour, and the two error
paths.


## [0.7.6] - 2026-06-21

> syslog TLS folded into `syslog_tcp` on both sides (output: per-peer,
> input: optional block); `otlp_http` gains TLS / mTLS; `output kafka`
> gains TLS / mTLS / SASL; `output otlp` split into `otlp_http` /
> `otlp_grpc` and both gain per-peer rotation + mTLS; `output http`
> gains per-peer rotation + mTLS

### Added — `output http` per-peer rotation + mTLS

`output http` now accepts a `peer { url tls{...} }` (single destination
shorthand) or `peers { peer { url tls{...} } ... }` (multi-destination)
block in place of the previous top-level `url`. On each send the
rotation picks the next available peer (cooldown expired) and tries
it; a peer that fails the request is marked cooled-down for the
shared 5-second window and skipped on subsequent sends until the
cooldown expires. When every peer is currently cooled the rotation
falls back to the cursor start — the queue layer's per-event retry
then handles longer-term re-delivery (consistent with the existing
`output http` retry semantics, which never had an internal retry
loop).

Per-peer `tls { ca cert key }` enables mTLS. `cert` and `key` are
paired (both-or-neither, enforced at parse time by
`ClientTlsConfig::validate`). PEM files for the cert and key are
loaded once at startup; chmod 600 the key, the daemon already refuses
to run as root.

This is a **breaking change** for any existing `output http` config
that used a single top-level `url`:

```text
# before
def output es {
    type http
    url "https://es:9200/_bulk"
    tls { ca "/etc/limpid/ca.crt" }
}

# after (single peer — shorthand mirrors output syslog_tcp / otlp_http)
def output es {
    type http
    peer {
        url "https://es:9200/_bulk"
        tls { ca "/etc/limpid/ca.crt" }
    }
}

# after (round-robin across multiple endpoints)
def output es {
    type http
    peers {
        peer { url "https://es01.example.com:9200/_bulk"; tls { ca "/etc/limpid/ca.crt" } }
        peer { url "https://es02.example.com:9200/_bulk"; tls { ca "/etc/limpid/ca.crt" } }
    }
}
```

`verify` stays top-level — disabling certificate validation is an
output-wide debug switch, not a per-peer one. `method`,
`content_type`, `compress`, `headers`, `batch_size`, `batch_timeout`
also remain top-level (they apply across all peers).

### Added — `output otlp_http` / `output otlp_grpc` per-peer rotation + mTLS

Both OTLP output transports now accept a `peers { peer { endpoint
tls{...} } ... }` block in place of the previous top-level `endpoint`.
On each flush the rotation tries peers in round-robin order; a peer
that fails the request is cooled-down for the standard 5-second
window (shared with the syslog outputs) and skipped on subsequent
flushes until the cooldown expires. Inside one flush the `retry
{ … }` budget still governs total attempts, but the rotation
transparently picks the next available peer for each retry.

Per-peer `tls { ca cert key }` enables mTLS. `cert` and `key` are
paired (both-or-neither, enforced at parse time); `ca` alone adds a
custom CA on top of the system root store. PEM files for the cert and
key are loaded once at startup; chmod 600 the key, the daemon already
refuses to run as root.

This is a **breaking change** for any existing `output otlp_http` or
`output otlp_grpc` config that used a single top-level `endpoint`:

```text
# before
def output o {
    type otlp_http
    endpoint "https://collector.example.com:4318/v1/logs"
    tls { ca "/etc/limpid/ca.crt" }
}

# after
def output o {
    type otlp_http
    peers {
        peer {
            endpoint "https://collector.example.com:4318/v1/logs"
            tls { ca "/etc/limpid/ca.crt" }
        }
    }
}
```

The shared `crate::tls::TLS_CLIENT_BLOCK_PROPERTIES` schema was
extended from `ca`-only to `ca` / `cert` / `key` (all optional, with
the paired invariant enforced by `ClientTlsConfig::validate`).
`output syslog_tcp` (per-peer) and `output kafka` were both already
carrying their own ca/cert/key block constants and have been migrated
to the shared schema — no user-visible config change for those two,
but the duplicated `PropertySpec` definitions are gone.

### Changed — `output otlp` split into `output otlp_http` and `output otlp_grpc` (breaking)

The single `output otlp { protocol grpc | http_* }` module is replaced
by two independent modules — one per transport. The DSL no longer has a
`protocol` switch that flips request-shape, header semantics, and
endpoint conventions inside the same module.

Migration:

```text
# before (0.7.5)                 # after (0.7.6+)
def output o {                   def output o {
    type otlp                        type otlp_http        # or otlp_grpc
    protocol "http_protobuf"         protocol "http_protobuf"   # otlp_http only;
    endpoint "..."                   endpoint "..."             # otlp_grpc has no `protocol`
    ...                              ...
}                                }
```

Old configs (`type otlp` + `protocol grpc | http_*`) are rejected at
startup. Wire-level behaviour is unchanged — the existing
`ExportLogsServiceRequest` encoding, retry semantics, `batch_level`
merging, headers / metadata handling, and TLS surface all carry over
byte-for-byte. Only the DSL surface and module registration changed:
the shared bits live under `crates/limpid/src/modules/output/otlp/`
(internal helpers), and the public modules are
`output/otlp/http.rs` (`OtlpHttpOutput`, `type otlp_http`) and
`output/otlp/grpc.rs` (`OtlpGrpcOutput`, `type otlp_grpc`), mirroring
the input side which has shipped split modules since 0.7.0.

Why split rather than keep one knob: every `protocol`-conditional
property — `headers` (HTTP) vs gRPC metadata, `verify false` (HTTP
only — tonic refuses), endpoint path conventions, compression sets,
peer round-robin semantics (the future addition) — turned into a
`protocol`-dependent check at parse time and a footnote in docs.
Splitting collapses each module's surface to what its transport
actually supports.

### Added — `output kafka` `tls { ... }` and `sasl { ... }` blocks

`output kafka` now accepts optional `tls { ca cert key }` and
`sasl { mechanism username password_file }` blocks. The
`security.protocol` is derived from which blocks are present
(`plaintext` / `ssl` / `sasl_plaintext` / `sasl_ssl`), so the most
common production setup (SASL/SCRAM over TLS) is a single config
change away.

`cert + key` in the `tls` block are both-or-neither: present them
together for mTLS, omit both for one-way TLS. `ca` alone is fine for
private-CA broker certs.

Supported SASL mechanisms: `plain`, `scram_sha_256`, `scram_sha_512`.
The DSL ident grammar forbids `-`, so the SCRAM mechanisms are spelled
with underscores in the config and mapped to librdkafka's hyphen
spelling (`SCRAM-SHA-256` / `SCRAM-SHA-512`) at parse time.

SASL credentials are split intentionally: `username` is inline (not
secret), `password_file` points to a separate file (chmod 600) — the
same disposition limpid uses for TLS private keys. Inline `password`
is **not** supported, so credentials never end up in config diffs,
backups, or pretty-printed log output. Empty `password_file` is
rejected as a misconfiguration.

`brokers` is still a single comma-separated bootstrap list — librdkafka
handles broker discovery / partition routing / leader failover
internally, so unlike the syslog / http / otlp outputs there is no
per-peer rotation layer to add here.

```
def output secure {
    type kafka
    brokers "kafka1.example.com:9094"
    topic "syslog-events"
    tls { ca "/etc/limpid/kafka-ca.pem" }
    sasl {
        mechanism scram_sha_512
        username "limpid-producer"
        password_file "/etc/limpid/kafka.pw"
    }
}
```

### Added — `input otlp_http` optional `tls { ... }` block (HTTPS + mTLS)

`input otlp_http` now accepts the same `tls { cert key ca }` block that
`input syslog_tcp` and `input otlp_grpc` already use. With the block
present, the listener accepts HTTPS only (no HTTP fallback on the same
port). `ca` enables mTLS — clients without a valid certificate signed
by the configured CA are rejected at handshake.

The OTLP/HTTP default port (4318) is unchanged regardless of the block;
there is no separate "secure" port in the OTLP spec.

```
def input otlp_in {
    type otlp_http
    tls {
        cert "/etc/limpid/cert.pem"
        key  "/etc/limpid/key.pem"
        ca   "/etc/limpid/client-ca.pem"   # optional; enables mTLS
    }
}
```

Internals: `otlp_http` now drives the axum `Router` through the
`axum-server` crate (the bundled `axum::serve` is hardcoded to
plaintext `TcpListener` in 0.7), giving the same HTTP/1+2 + graceful
shutdown shape on both transports.

### Changed (BREAKING) — `output syslog_tls` removed, TLS is now per-peer on `syslog_tcp`

The standalone `output syslog_tls` module that shipped in 0.7.4 is
removed. The `output syslog_tcp` module now accepts a per-peer `tls`
block (inline or named-profile reference); peers without `tls` use
plaintext on the same output. A single relay can therefore fan out to
a mix of TLS-encrypted and plain destinations.

Default port is per-peer: 6514 (RFC 5425) when `tls` is set on that
peer, 514 (RFC 6587) otherwise.

Migration — rename `type syslog_tls` to `type syslog_tcp`. The existing
top-level `tls { profile { ca cert key } }` map and the per-peer
`tls { ... }` / `tls <profile_name>` forms work as-is:

```diff
def output secure {
-    type syslog_tls
+    type syslog_tcp
    framing octet_counting
    tls {
        corporate_ca { ca "/etc/limpid/corp-ca.pem" }
    }
    peers {
        peer { host "a.example.com" tls corporate_ca }
        peer { host "b.example.com" tls corporate_ca }
    }
}
```

### Changed (BREAKING) — `input syslog_tls` removed, TLS is now an optional block on `input syslog_tcp`

The standalone `input syslog_tls` module is removed; `input syslog_tcp`
now accepts an optional `tls { cert key ca }` block. mTLS (client cert
verification) is enabled by setting `ca` in the block — exactly the
same shape as `input otlp_grpc`, which has worked this way since 0.7.0.

Default bind port flips with the block: **6514** (RFC 5425) when
`tls` is configured, **514** (RFC 6587) otherwise.

Migration — rename `type syslog_tls` to `type syslog_tcp`. The existing
`tls { ... }` block works as-is:

```diff
def input secure {
-    type syslog_tls
+    type syslog_tcp
    tls {
        cert "/etc/limpid/certs/server.crt"
        key  "/etc/limpid/certs/server.key"
        ca   "/etc/limpid/certs/client-ca.crt"   # mTLS
    }
}
```

A latent rustls panic (`CryptoProvider not installed`) that triggered
when running `input syslog_tls` alone is fixed as a side effect — the
new `syslog_tcp` code calls `install_default_crypto_provider()` before
the rustls server config is built.

## [0.7.5] - 2026-06-07
> array primitives and expression chaining

### Added — block-argument array primitives

Arrays can now be transformed with expression-level block arguments:
`map(array) { |x| ... }`, `filter(array) { |x| ... }`,
`find(array) { |x| ... }`, and `reduce(array, init) { |acc, x| ... }`.
The block body follows the same expression-function shape as
`def function`: optional `let` bindings followed by a required return
expression. Block locals are scoped to the block evaluation and do not
leak into event workspace.

### Added — expression pipe operator

The `|>` operator chains expression-shaped transforms by inserting the
left-hand value as the first argument to the function on the right.
For example, `events |> filter { |e| e.kind == "auth" } |> map { |e| e.user }`
is parse-time sugar for nested ordinary function calls; no runtime pipe
object is introduced.

### Added — array helper primitives

New collection helpers cover common whole-array operations:
`first`, `last`, `concat`, `distinct`, `sum`, `max`, `min`, `entitle`,
`path`, and `is_array`. Existing `append`, `prepend`, and `len` remain.

### Changed (BREAKING) — remove `find_by` and statement-form `foreach`

`find_by(array, key, value)` is removed in favour of
`find(array) { |x| x.key == value }`, which supports arbitrary
predicates. Statement-form `foreach` and the magic `workspace._item`
binding are removed; use `map`, `filter`, `find`, or `reduce` instead.

## [0.7.4] - 2026-06-03
> multi-destination syslog outputs + TLS

### Added — `syslog_tls` output

A new output module sends syslog over TLS-encrypted TCP. Default port
is 6514 (RFC 5425). Supports server verification against a custom CA
or the Mozilla root store, and optional mutual TLS via a client
certificate. Named TLS profiles can be defined at the output level and
referenced from individual peers; per-peer inline TLS blocks are also
supported.

### Added — multi-destination peer lists with round-robin failover

The `syslog_tcp`, `syslog_udp`, and new `syslog_tls` outputs now accept
a `peers { peer { ... } ... }` block in addition to the single `peer
{ ... }` form. Events are distributed across peers in round-robin
order. A peer that returns a send, connect, or (for TLS) handshake
error is taken out of rotation for a 5-second cooldown; the existing
queue layer handles retry when every peer is unavailable.

### Changed (BREAKING) — output module rename: tcp/udp → syslog_tcp/syslog_udp

The `output` modules previously named `tcp` and `udp` are renamed to
`syslog_tcp` and `syslog_udp`, matching the input-side naming. Both
modules have always implemented RFC 6587 syslog framing, so the new
names are honest about their scope. No alias is retained.

Configs that used `type tcp` or `type udp` in `def output { ... }`
must be updated:

    -    type tcp
    +    type syslog_tcp

    -    type udp
    +    type syslog_udp

### Changed (BREAKING) — DSL: `address` / `host`+`port` replaced by `peer { ... }`

The top-level `address "host:port"` (and `host` + `port`) properties
on `syslog_tcp` and `syslog_udp` are removed. Configs must use the
new `peer { host port }` form (single destination) or
`peers { peer { ... } ... }` (multiple). Mixed-form configs are
rejected by the schema validator.

    -    type syslog_tcp
    -    address "10.0.0.1:514"
    +    type syslog_tcp
    +    peer { host "10.0.0.1" port 514 }

## [0.7.3] - 2026-05-17
> property-schema parity — `--check` and runtime now read the same surface

### Fixed — `--check` OK / runtime fail asymmetry on every `def input` / `def output`

0.7.2's declarative property schema was applied at two points: the analyzer
(`--check`) and the runtime (`ModuleRegistry::create_input` / `create_output`).
The analyzer stripped the structural `type` key before validating against the
Module's schema; the runtime did not. The result was that every config with a
`type tcp` (or any other type) line passed `--check` cleanly but was rejected
by the daemon at startup with:

    output 'forwarder' (type 'tcp') has invalid configuration:
      - unknown property 'type' — aborting startup

The fix is structural. A new `ModuleProperties` type extracts `type` into a
typed slot at parse time, and the Module trait's `from_properties` /
`ModuleRegistry::create_*` factory closures both consume only
`properties.user_properties()` — there is no `Vec<Property>` view that still
contains `type` for anyone to forget to strip. The bug class is impossible to
re-introduce without changing the type signatures.

`property_schema()`'s contract is unchanged; every Module schema continues to
describe its own user properties only. Configs that pass `--check` on 0.7.2
now also start the daemon on 0.7.3 — no operator action required beyond
upgrading the binary.

### Fixed — missing `type` is now a parse-time error

Previously `def input foo { ... }` without a `type` key was silently skipped
by `module_props.rs` and surfaced as a confusing "input '...' has no type"
error only at daemon start. The parser now constructs `ModuleProperties` for
every def block; a missing, duplicated, or non-ident `type` becomes a
parse-time error with the def name in the message:

    input 'foo': missing required property 'type'

Same loudness as a syntax error, same location attribution.

## [0.7.2] - 2026-05-17
> declarative property schema — `--check` now loudly rejects every config typo

`--check`'s coverage extended from pipeline / process DSL to every
property surface in the configuration: Module properties on `def
input` / `def output`, their nested sub-blocks (`queue`, `tls`,
`retry`, `headers`), and the top-level `control` / `geoip` / `table`
blocks. Each Module advertises its accepted shape as a
`&'static [PropertySpec]`; the analyzer and the runtime read the same
declaration, so unknown keys, type-mismatched values, out-of-set enum
values, and missing required fields surface as rustc-style errors
with `did you mean ...?` suggestions instead of being silently
defaulted away.

### Added — `dsl::schema` + per-Module `property_schema()`

- New `dsl::schema` module declares `PropertyValueKind`
  (`String | Int | Bool | Duration | Size | Enum | Block | StringMap`)
  and `PropertySpec`. Modules splice these into a single static
  schema; `dsl::schema::validate` walks any property surface against
  it and collects every finding in one pass.
- `Module::property_schema()` trait method (default `None` for a
  gradual migration; every built-in carries a schema after this
  release). `Module::build()` is the convenience entry that runs
  validation before construction for direct callers (tests, snippet
  libraries) — the `ModuleRegistry` does the equivalent step at
  `create_input` / `create_output` time.
- Shared `queue::QUEUE_PROPERTY_SPEC` declared once next to the
  queue parser and spliced into every output schema, so the
  `queue { type | path | max_size | capacity }` block is checked
  uniformly across all sinks.

### Added — analyzer wiring

- New `check::module_props` pass validates every `def input` / `def
  output` against the Module's schema, including a Levenshtein-based
  did-you-mean hint drawn from the registered type names when
  `type tcsp` or similar misses every known module.
- New `check::global_props` pass applies the same treatment to the
  top-level `control { socket | error_log }`, `geoip { database }`,
  and `table { <name> { load | max | ttl } }` blocks.
- New `DiagKind::PropertySchema` keeps these findings filterable
  separately from existing `UnknownIdent` / `TypeMismatch` /
  `Dataflow` categories.

### Fixed — `framing non_transparent` false-positive

The expression-level "unknown identifier" walk on output properties
used to flag legitimate bare-ident enum values (`framing
non_transparent`, `queue { type disk }`) as unbound — it had no idea
the bare ident was an enum member. The walk now consults the
Module's schema and skips its own shape check for keys the schema
already owns. Workspace references inside template values
(`address "${workspace.x}:1"`) are *not* skipped — the dataflow
reference check still applies on every property regardless of
schema coverage.

### Fixed — silent fallback on unknown enum values

Previously `framing non_trasnaprent` (a typo) silently fell back to
the default framing. The schema layer rejects unknown enum values at
both `--check` time and runtime startup, with a `did you mean ...?`
hint.

### Fixed — `include` matching zero files is no longer silent

`include "path/that/does/not/exist.limpid"` (and any glob that
expands to zero matches) used to pass `--check` silently and then
surface at runtime as confusing "unknown process" errors with no
obvious tie back to the typo'd include line. The loader now bails
loudly with `include path '...' (resolved to '...') matched no
files` at config-load time, before `--check` even runs the analyzer.
Same posture as rsyslog / syslog-ng on a missing include directive.

### Security / hardening

- **Daemon mode now refuses to start as root (euid 0).** limpid is a
  network-listening daemon and an event-processing engine; both
  surfaces have meaningful blast radius if compromised, so the
  principle is "drop privileges before reading any event". The
  canonical operational shape is systemd `User=limpid` plus
  `AmbientCapabilities=CAP_NET_BIND_SERVICE` for listeners on
  privileged ports (< 1024). The check applies only to daemon mode;
  `--check` / `--test-pipeline` / `--graph` are read-only and run
  fine as root. Operators who genuinely need to run the daemon as
  root (containerised init, debugging) can set
  `LIMPID_ALLOW_ROOT=1` to override.

### Notes

- Configs that pass `--check` today still pass. Configs that
  previously slipped through with silent fallbacks (typo'd keys,
  unknown enum values, mis-typed `type` ident) now fail loudly. This
  is a bug fix, not a breaking change in the semver sense — the
  previous behaviour silently corrupted the operator's intent.
- `dsl::schema::levenshtein` consolidates the two
  Levenshtein implementations the codebase used to carry; the
  analyzer's `suggestions` module now re-exports the same routine
  the schema validator uses.

## [0.7.1] - 2026-05-17
> journal input LOTL + transport-agnostic vocabulary parsers + datetime primitives + additional SIEM and OSS NDR parsers (real-traffic verified)

The journal input is rewritten to emit `journalctl -o json`-equivalent
JSON on `ingress`, replacing the synthesised
`"IDENTIFIER[PID]: MESSAGE"` string and the silent loss of every
non-MESSAGE journald field. Downstream snippets (`parse_journald`,
`parse_openssh`, `parse_sudo`, etc.) now see PRIORITY, _PID,
_HOSTNAME, __REALTIME_TIMESTAMP, _SYSTEMD_UNIT, _SELINUX_CONTEXT,
and the rest by their journald-canonical names.

Vocabulary parsers (`parse_openssh`, `parse_sudo`, `parse_postfix`,
`parse_combined_log`) are decoupled from their transport. Each now
reads from a vocabulary-named workspace namespace (`workspace.openssh.*`,
`workspace.sudo.*`, …) that the pipeline writer populates via an
inline bridge from whichever transport actually arrived. The
vocabulary parser does not enumerate transports — that knowledge
stays in the pipeline. OCSF records grow `time`, `device.hostname`,
and `actor.process.pid` from the trusted source the transport
provides.

Plus three new datetime parsers: `parse_datetime_rfc3339` and
`parse_datetime_rfc2822` as Rust primitives, `parse_datetime_rfc3164`
as an LPL snippet. The split mirrors the design principle line —
spec'd atomic parsers live in Rust; policy / heuristic / fallback
live in LPL.

### Fixed — journal input is dumb transport again (Principle 2)

`crates/limpid/src/modules/input/journal.rs` previously synthesised
`"IDENTIFIER[PID]: MESSAGE"` on `ingress` and discarded every other
journald field. The synthesis violated Principle 2 (input is dumb
transport, no interpretation) and forced every downstream `wrap_*`
process to re-extract pid / identifier with regex against the
synthesised string. PRI on the wire was lost entirely because
facility/severity were thrown away by the input.

Live off the land: `ingress` is now byte-equivalent to one line of
`journalctl -o json`. All enumerated data fields are preserved
under their journald-canonical names; trusted-address metadata
(`__CURSOR` / `__REALTIME_TIMESTAMP` / `__MONOTONIC_TIMESTAMP`) is
surfaced via the libsystemd metadata APIs. Non-UTF-8 byte values
become JSON arrays of integers (journalctl convention).

`__SEQNUM` / `__SEQNUM_ID` are not surfaced — the `systemd-0.10.x`
crate exposes no equivalent API. Add when upstream support lands.

Workspace stays empty on input. Downstream snippets
(`parse_journald` etc.) decode the JSON in the process layer.

**Breaking**: any pipeline that consumed the old synthesised string
needs to switch to `process parse_journald` + an inline bridge.

### Added — datetime parser primitives

Three layered datetime parsers, picked by what the wire actually
carries:

- **`parse_datetime_rfc3339(text)`** — Rust primitive. Strict
  internet profile of ISO 8601 used by RFC 5424 syslog, OTLP, OCSF
  `time`, AWS CloudTrail `eventTime`, and most modern cloud audit
  logs. Accepts `Z` / `±HH:MM` / `±HHMM` transparently — solves the
  `strptime("...Z", "%z")` gotcha (`chrono` rejects the bare `Z`
  literal under `%z`).
- **`parse_datetime_rfc2822(text)`** — Rust primitive. Email `Date:`
  headers and legacy HTTP-date-style wires.
- **`parse_datetime_rfc3164(text)`** — LPL `def function` shipped as
  `packaging/snippets/functions/parse_datetime_rfc3164.limpid`. RFC
  3164 wire (`Apr 30 01:23:45`) carries neither year nor timezone;
  the parser encodes the standard policy (current-year +
  future-clamp + UTC assumption — what rsyslog / syslog-ng /
  Vector / Fluent Bit all converge on) in DSL so operators on
  non-UTC senders can fork and edit without a rebuild.

### Added — additional vendor parsers

Five additional vendor / vocabulary parsers covering Juniper SRX,
Check Point, Trellix NSP, Sysmon, BIND, and auditd:

| Parser | Source | OCSF class(es) |
|---|---|---|
| `parse_juniper_srx_sd_syslog` | Juniper Junos SRX in `set security log format sd-syslog` mode — covers all daemons that emit a `[junos@<EID> ...]` SD block: **RT_FLOW** (SESSION_CREATE/CLOSE/DENY + APPTRACK_SESSION_*), **RT_IDP** (IDP_ATTACK_LOG_EVENT + IDP_APPDDOS_*), **RT_IDS** (RT_SCREEN_*), **RT_UTM** (AV / Antispam / Content / Webfilter), **RT_AAMW** (Sky ATP), **RT_SECINTEL** (threat-feed). Verified against the elastic/integrations juniper_srx corpus (66/66 emit, 0 error) | 4001 / 2004 / 4002 |
| `parse_juniper_srx_syslog` | Juniper Junos SRX RT_IDP / IDP_ATTACK_LOG_EVENT (RFC 3164 unstructured syslog — `set security log format syslog` default mode) — real-traffic verified | 2004 |
| `parse_nsp` | Trellix / McAfee Network Security Platform (NSP) IPS alerts. **Real-traffic verified**: 72/72 alerts emit cleanly across HTTP / SSH / SSL / NETBIOS-SS / TELNET / NTP / BACKDOOR categories. Real wire turned out to emit unquoted multi-word values (`attack_name=NETBIOS-SS: Windows SMB Remote Code Execution Vulnerability` without the documented quotes); the parser now uses a single fixed-order regex over the full Trellix standard template, which is the only robust extraction strategy for unquoted KV with embedded spaces | 2004 |
| `parse_checkpoint_leef` | Check Point LEEF 2.0 traffic events (Accept / Drop / Reject / Block) inside a syslog wrapper. Renamed from `parse_checkpoint`; targets the LEEF wire format used by QRadar bridges. Synthetic-verified only | 4001 |
| `parse_checkpoint_syslog` | Check Point Syslog Exporter wire format (`[key:"value"; ...]` SD with `sys_message::"..."` double-colon convention; also handles R81+ `Log [Fields@<EID> ...]` `=` variant). **Real-corpus verified** against elastic/integrations checkpoint (91/91 events emit across firewall / threat / auth / audit dispositions) | 4001 / 2004 / 3002 |
| `parse_sysmon` | Microsoft Sysmon EventID 1 (ProcessCreate) / 3 (NetworkConnect) / 11 (FileCreate), as JSON via NXLog / Vector / Winlogbeat. Synthetic-verified only — the elastic sysmon_linux corpus uses a different field-path convention (`winlog.event_id` / `winlog.event_data`) so it cannot exercise this parser as-is | 1007 / 4001 / 1001 |
| `parse_bind` | ISC BIND 9 `querylog` text format (`category queries`). Synthetic-verified only — no public corpus discovered for the format | 4003 |
| `parse_auditd` | Linux auditd, covers ~45 type codes across 7 OCSF classes (3002 Authentication / 3001 Account Change / 1007 Process Activity / 1001 File System / 4001 Network / 2002 Vulnerability Finding / 2004 Detection Finding). Handles `node=<host>` prefix injected by RHEL `audisp-remote` dispatcher. **Real-corpus verified** against elastic/integrations auditd (68/69 emit, 1 corrupt record errors loudly) | 3002 / 3001 / 1007 / 1001 / 4001 / 2002 / 2004 |

Junos security logs ship in two distinct wire formats — the
`sd-syslog` structured form is rare in practice (most SRX
deployments stay on the default `syslog` mode), so both formats
get a dedicated parser per the library's one-file-per-
`(vendor, format)` convention. `parse_juniper_srx_sd_syslog` is
synthetic-verified only; `parse_juniper_srx_syslog` is verified
against live RT_IDP traffic (RT_IDP / IDP_ATTACK_LOG_EVENT → OCSF
2004 Detection Finding with `finding_info` / `attacks` /
`connection_info` populated).

Each parser follows the v0.7.1 intake-schema convention
(`workspace.<vocab>.{body, …}` with hostname / time from the
upstream bridge) and surfaces `device.hostname` and
`actor.process.pid` in the emitted OCSF record where the wire
provides them. Coverage scopes are documented per file — only the
authentication subset of auditd's type codes is in scope this
release (USER_LOGIN / USER_AUTH / USER_ACCT / USER_LOGOUT /
CRED_ACQ / CRED_DISP); SYSCALL / EXECVE / PATH multi-record
assembly is out of scope.

Compose_ocsf leaves extended for the new parsers' fields:
- 1001 / 1007 / 4001 / 4003 gain `status_id`, `actor`, `device`,
  and `unmapped` forwarding
- 2004 Detection Finding gains `status_id`, `connection_info`,
  `device`, `actor`, `attacks`, and `unmapped` forwarding (for
  the new Juniper SRX IDP parser)

Real-corpus verification pass on elastic/integrations and (for
NSP) real wire traffic completes the trustworthiness story for
most of these vendor parsers — every parser's header
`Coverage scope` section now lists the exact corpus / dataset it
was exercised against, and what residual gaps remain (Sysmon and
BIND have no usable public corpus; CheckPoint LEEF has no
public corpus distinct from the Syslog Exporter form).

### Added — OSS NDR parsers (Suricata + Zeek)

The de-facto open-source NDR pair. Suricata raises alerts via
signatures; Zeek records per-protocol telemetry exhaustively.
Operators deploying either ship event volumes that dwarf any
single vendor source, so both get first-class snippet coverage.

| Parser | Source | OCSF class(es) |
|---|---|---|
| `parse_suricata` | OISF Suricata Extensible Event Format (EVE) JSON, dispatched by `event_type`: alert → 2004, dns → 4003, http → 4002, flow / tls / fileinfo → 4001, stats → drop. **Real-corpus verified** against elastic/integrations suricata (61/63 emit + 1 stats drop + 1 corrupt JSON in corpus) | 2004 / 4001 / 4002 / 4003 |
| `parse_zeek_default` | Zeek default-enabled scripts: conn / dns / http / ssl / files / x509 / weird / notice. **Real-corpus verified** against elastic/integrations zeek (61/61 emit, all 8 streams) | 2004 / 4001 / 4002 / 4003 |
| `parse_zeek_soc` | Adds auth / protocol scripts most SOC deployments enable: ssh / smtp / ftp / dhcp / kerberos / ntlm / radius / smb_{mapping,cmd,files} / dce_rpc / snmp / rdp. Transitively includes `parse_zeek_default`. **Real-corpus verified** (85/85 emit, 20 distinct streams) | + 3002 / 4004 / 4005 / 4006 / 4007 / 4008 / 4009 |
| `parse_zeek_full` | Adds the rest (signature / intel / traceroute / tunnel / pe / mysql / irc / sip / dnp3 / modbus / socks / syslog / ntp / ocsp / rfb / dpd) + drops low-value operational streams (stats / capture_loss / known_hosts / known_services / known_certs / software) + a **catch-all** that wraps any remaining unknown `_path` into a 4001 record's `unmapped` (zero data loss guarantee). Transitively includes `parse_zeek_soc`. **Real-corpus verified** against the full 43-stream elastic/integrations zeek corpus (120/135 emit + 15 expected drops) | + catch-all |

Zeek's scope layering is **nested**: an operator picking
`parse_zeek_soc` automatically gets default coverage (the SOC
file includes default); picking `parse_zeek_full` gets soc +
default + everything else. One include line, one process name
in the pipeline.

Each Zeek scope file also ships **convenience entry points** with
`_native` / `_flat` suffixes that fold the intake step into the
parser itself:

- `_native` — Zeek's own JSON output (5-tuple nested under `id`),
  the expected production shape.
- `_flat` — Filebeat / Logstash-flattened form
  (`"id.orig_h"` etc.), the dotted-keys shape downstream ES
  pipelines emit. Runs `nest_dotted_keys` first to recover the
  native nested shape before dispatch.

Pipeline becomes a single stage: `process parse_zeek_soc_native
| compose_ocsf` (no separate `process { workspace.zeek = ... }`
intake block needed).

Suricata's EVE format does not vary by downstream shipper, so it
ships only one entry point following the existing
intake-separate convention.

### Added — `nest_dotted_keys` primitive

Some upstreams (Filebeat / Logstash JSON emitters used by zeek
and suricata modules, certain Splunk HEC sources, OpenSearch
ingest pipelines) flatten nested JSON for Elasticsearch indexing
conventions: `{"id": {"orig_h": "1.1.1.1"}}` becomes
`{"id.orig_h": "1.1.1.1"}`. The limpid DSL deliberately does
not expose bracket-subscript access (`body["id.orig_h"]`), so
dotted keys are unreachable from a parser without normalising
first.

`nest_dotted_keys(obj)` recursively un-flattens dotted keys
back into nested Objects, with loud-fail on collisions
(`{"a": 1, "a.b": 2}` errors out clearly). Generic across
vendors — used by parse_zeek_*_flat variants, and equally
applicable to any other Filebeat-flattened JSON.

### Added — shared `proto_num` / `http_method_activity_id` LPL helpers (DRY across vendor parsers)

Two cross-vendor OCSF mappings were duplicated across parsers (9 ×
`*_proto_num` and 3 × `*_http_activity_id`), each with the same
semantic body. Extracted to two shared LPL functions:

- `packaging/snippets/functions/proto_num.limpid` — IANA protocol
  number lookup, case-insensitive (`lower()`-folded), covering
  tcp / udp / icmp / icmpv6 / sctp / gre / esp / ah. Replaces
  `zeek_proto_num` / `suricata_proto_num` /
  `checkpoint_{syslog,leef}_proto_num` /
  `juniper_srx_{sd_syslog,syslog}_proto_num` /
  `paloalto_{syslog,cef}_proto_num` / `sysmon_proto_num`.
- `packaging/snippets/functions/http_method_activity_id.limpid` —
  HTTP request method → OCSF 4002 activity_id, the spec-standard
  mapping. Replaces `suricata_http_activity_id` /
  `zeek_http_activity_id` / `combined_log_activity_id`.

Parsers now `include "../functions/<name>.limpid"` and call the
shared helper directly. Behaviour identical (case-insensitive
proto_num is a superset that accepts every previous vendor-specific
case style; HTTP method mappings were already byte-identical across
the three sites). 12 callsites updated, ~80 lines of duplicated
helper deleted.

### Fixed — `parse_auditd` system-lifecycle section header drift

The "1008 Application Lifecycle" section header in `parse_auditd.limpid`
suggested the function emitted `class_uid: 1008`, but the code
emits 1007 Process Activity (OCSF 1008 is actually Windows Registry
Key Activity, not application lifecycle, which was confirmed during
the auditd parser was first written). Header rewritten with the
correct class plus the rationale.

### Fixed — `nest_dotted_keys` enforces depth limits (stack-overflow DoS mitigation)

`nest_dotted_keys` walked dotted keys and nested values recursively
with no depth bound. An attacker-controlled JSON like
`{"a.a.a...(100K dots)": 1}` would have recursed 100,000 deep into
`insert_path` and overflowed the thread stack — a denial-of-service
on any pipeline exposed to untrusted input (Zeek `_flat` operators
ingesting Filebeat-processed logs are the natural attack surface).

Two limits added:

- `MAX_DOTTED_DEPTH = 32` — segment count per dotted key. Filebeat
  / Logstash typically flatten 2-4 levels, so 32 leaves headroom
  without enabling unbounded recursion.
- `MAX_VALUE_DEPTH = 64` — defence-in-depth bound for `nest()`
  walking into Object / Array values. `parse_json` already caps JSON
  parse depth at 128 (serde_json default), but this protects calls
  from other Value sources.

Both limits raise loud parser errors (route to error_log) rather
than panic. Three new tests cover at-the-limit, above-the-limit,
and value-nesting cases.

### Fixed — `parse_datetime_rfc3339` accepts `±HHMM` offset (was strict colon-only)

`chrono`'s `parse_from_rfc3339` is strict per RFC 3339 and
requires the offset as `±HH:MM` (colon form) or `Z`. Many real
emitters (Suricata EVE, journald JSON export, `jq -r` default,
some CloudTrail regions) omit the colon and emit `±HHMM`. The
primitive's doc claimed both forms were accepted, but the
existing implementation only called the strict parser, so
`±HHMM` bodies routed silently to error_log.

The primitive now composes a small fallback chain:
`parse_from_rfc3339` → on failure → `parse_from_str` with `%z`
(which accepts both shapes). Documented surface is therefore
exactly `Z` / `±HH:MM` / `±HHMM`; deviations (space separator
instead of `T`, ISO 8601 basic form without dashes, abbreviated
offset `+09`, named zones) remain rejected and must be
normalised upstream.

### Added — transport parsers + RFC 5424 composer

Three new snippets that pair with the journal LOTL fix to express
transport stacking explicitly in pipelines:

- **`parsers/parse_syslog.limpid`** — thin wrapper around the
  `syslog.parse(ingress)` primitive that populates
  `workspace.syslog.*`. Lets a pipeline write
  `process parse_syslog | <bridge> | parse_<vocabulary>` rather
  than having every vocabulary parser inline its own
  `syslog.parse(ingress)` call.
- **`parsers/parse_journald.limpid`** — `workspace.journald =
  parse_json(ingress)`. Pairs with the LOTL change to expose all
  journald fields downstream by their canonical names.
- **`composers/compose_rfc5424.limpid`** — `workspace.journald.*` →
  RFC 5424 syslog wire. Replaces the hand-rolled `wrap_*` patterns
  on edge boxes that previously synthesised a frame from a
  regex-parsed string. Preserves the originating host via
  `coalesce(workspace.journald._HOSTNAME, hostname())` so relayed
  events keep their source identity instead of being stamped with
  the relay's hostname.

### Changed — vocabulary parser intake schemas

`parse_openssh`, `parse_sudo`, `parse_postfix`, and
`parse_combined_log` no longer call `syslog.parse(ingress)`
internally. Each reads from an explicit intake schema under its
vocabulary namespace that the pipeline writer populates:

| Parser | Intake schema |
|---|---|
| `parse_openssh`     | `workspace.openssh.{body, pid, hostname, time}` |
| `parse_sudo`        | `workspace.sudo.{body, pid, hostname, time}` |
| `parse_postfix`     | `workspace.postfix.{body, hostname, time}` (pid lives inside the body's postfix tag) |
| `parse_combined_log`| `workspace.combined_log.{body, hostname}` (CLF carries its own time + IP) |

The bridge from a transport into the intake is a one-process
inline block in the pipeline; see each parser's file header for
worked syslog / journald / tail examples.

OCSF records emitted by these parsers now include `time`,
`device.hostname`, and `actor.process.pid` (where applicable) —
values come from the trusted source the transport provides
(journald `_PID` and `_HOSTNAME` are kernel-verified; syslog
procid / hostname are sender-claimed). `compose_ocsf` leaves for
3002 / 3003 / 4002 / 4009 are extended to forward `device` and
`actor` into the egress JSON.

`filter_openssh_journal` is rewritten to read
`workspace.journald.MESSAGE` (set by upstream `parse_journald`)
instead of doing `syslog.parse(ingress)` against a now-JSON
ingress.

**Breaking**: existing pipelines must insert
`process parse_syslog | { workspace.<vocab> = { … } } | parse_<vocab>`
or the journald counterpart ahead of the vocabulary parser. The
"just call `parse_<vendor>`" shortcut against syslog ingress no
longer works.

### Added — design rationale and snippet authoring convention

- `docs/src/design-principles.md` gains a new operating rule
  "Workspace is event-scoped, not message-passed". Records that
  `process A | B` is sequential composition over a shared workspace,
  not an object pipe. Cites the "openssh over CEF over syslog over
  JSON over OCSF over OTLP" stack as an example of where the library
  explicitly stops covering and pushes the wiring decision to the
  pipeline writer.
- `docs/src/processing/design-guide.md` codifies the `// Upstream:`
  header convention. A vocabulary parser binds implicitly to a finite
  set of upstream stacks; spelling them out in the file header is
  the closest we get to a checkable contract without growing the
  DSL.

### Notes

- DSL syntax: unchanged.
- `cargo build --release` green; `cargo test --workspace` green.
  Datetime primitives gained an extra `accepts_microsecond_fractional_no_colon`
  test for the Suricata-shape `±HHMM`+microsecond case. `nest_dotted_keys`
  ships with 9 unit tests covering simple nesting, sibling merging,
  three-level nesting, recursion into nested objects / arrays,
  leaf/branch collision rejection, empty-segment rejection, and
  pass-through of non-Object inputs.
- Snippet library now also ships `packaging/snippets/functions/`
  for LPL `def function` helpers (currently
  `parse_datetime_rfc3164.limpid`).
- Snippet file count this release: 4 Zeek scope files
  (`parse_zeek_default` / `parse_zeek_soc` / `parse_zeek_full` +
  the `_native` / `_flat` convenience variants live inside each)
  + 1 Suricata + 1 CheckPoint Syslog Exporter + 2 Juniper SRX
  format variants + 1 Trellix NSP + expanded `parse_auditd` /
  `parse_openssh` = 9 new parser files on top of the v0.7.0
  baseline.

---

## [0.7.0] - 2026-04-30
> snippet library v1 — 11 vendor parsers, OCSF 27-class composer; DSL fix for sub-process error propagation

The snippet library debut. Eleven vendor / format parsers ship,
covering the operational vocabulary of the dominant unix and
network-device log sources, plus a 27-class OCSF composer that maps
the parser-canonical `workspace.limpid.*` shape to OCSF 1.3.0 JSON
on `egress`. Operators can drop a single `include` into their
config and immediately ship vendor logs into a SIEM / data lake
in OCSF form.

Plus a DSL runtime fix that turned out to be load-bearing for the
nested-dispatch parsers in this library: `error` from inside a
sub-process now propagates correctly to the pipeline boundary
instead of being swallowed at the `process` call.

### Added — Snippet library

Eleven parsers in `packaging/snippets/parsers/` (installed under
`/usr/share/limpid/snippets/parsers/`):

| Parser | Source | OCSF class(es) emitted |
|---|---|---|
| **Security devices / cloud audit** | | |
| `parse_fortigate_cef` | FortiGate (CEF wrap) | 4001 / 2004 / 3002 / 6002 |
| `parse_fortigate_syslog` | FortiGate (native KV syslog) | (same as CEF) |
| `parse_paloalto_cef` | PAN-OS (CEF wrap) | 4001 / 2004 / 6004 / 3002 |
| `parse_paloalto_syslog` | PAN-OS (native CSV syslog) | (same as CEF) |
| `parse_asa` | Cisco ASA / FTD-in-ASA-mode (syslog) | 3002 / 4001 |
| `parse_cloudtrail` | AWS CloudTrail (JSON) | 6003 API Activity |
| **Server / host systems** | | |
| `parse_openssh` | OpenSSH `sshd` (syslog / journald) | 3002 Authentication |
| `parse_sudo` | sudo (syslog / journald) | 3003 Authorize Session |
| `parse_combined_log` | Apache / Nginx access log (combined format) | 4002 HTTP Activity |
| `parse_postfix` | Postfix MTA (syslog) | 4009 Email Activity |
| `parse_winevent_json` | Windows Security event log (NXLog / Vector / Winlogbeat JSON) | 3002 / 1007 / 3001 / 3006 |
| **Vendor-neutral** | | |
| `parse_ocsf` | OCSF JSON inbound (any vendor's prior compose_ocsf output) | passthrough (any class) |

Two composers in `packaging/snippets/composers/`:

- `compose_ocsf` — dispatches by `workspace.limpid.class_uid` to per-class
  leaves, covering the OCSF 1.3.0 priority set (27 classes: 1001 /
  1007 / 1008 / 1009 / 2002 / 2003 / 2004 / 2005 / 3001 / 3002 / 3003 /
  3005 / 3006 / 4001 / 4002 / 4003 / 4004 / 4005 / 4006 / 4007 / 4008 /
  4009 / 4010 / 6003 / 6004 / 6005 / 6007). Reads only
  `workspace.limpid.*` per the parser ↔ composer contract; vendor
  intermediates (`workspace.cef`, `workspace.syslog`) are not
  composer-visible.
- `compose_replayable` — minimal `{received_at, source, ingress}`
  shape that round-trips through `inject --json` for parser
  regression / replay capture.

One filter in `packaging/snippets/filters/`:

- `filter_openssh_journal` — drops `pam_unix(sshd:session): session
  opened/closed` PAM noise that journald sources before they reach
  `parse_openssh` (sshd already emits its own `Accepted ...` /
  `Disconnected ...` lines that cover the same authentication
  fact, so the PAM duplicate would double-count).

Field naming follows the parser ↔ composer contract:
`workspace.limpid.<canonical-OCSF-field>` — the parser picks vendor
fields off the wire and writes them to a single canonical scratch
namespace, the composer reads only that namespace and emits OCSF
JSON. Vendor intermediates (`workspace.cef`, `workspace.syslog`,
`workspace.pf`, etc.) are parser-private.

Verified against real / public test corpora where available
(playground sshd, FLAWS CloudTrail dataset, OTRF Mordor Windows
event JSON, miroslav-siklosi Cisco ASA syslog generator, real
Postfix mail.log slice). Each parser's docstring records the
specific dataset and its parse-rate, plus `NOTE`-flagged subtypes
that are documented but not yet exercised against live data.

### Fixed — sub-process `error` propagates past the `ProcessCall` boundary

`error` from inside a sub-process (`def process A { ... process B }`
where `B` fires `error`) was being swallowed at the caller's
`ProcessCall` arm in `crates/limpid/src/dsl/exec.rs`. Pre-fix the
caller restored the event from a workspace snapshot and continued
the pipeline as if nothing happened — making the operator-explicit
DLQ routing invisible at the pipeline boundary. Downstream
processes (typically `compose_ocsf`) then ran on the half-populated
workspace and produced a confusing secondary error like
`compose_ocsf: unsupported class_uid` that shadowed the original.

The fix removes the swallow: the sub-process Err propagates up
through `exec_process_body` to the pipeline-level handler, which
routes the event to the configured `error_log` (DLQ) exactly once
with the operator's original message intact, and the rest of the
pipe is skipped.

`try { process foo } catch { ... }` continues to work as before
for fail-soft on a specific call — the catch body now actually
runs after the sub-process error (pre-fix the swallow happened
before `try`/`catch` could see the Err).

The bug shipped in v0.5.5 (the release that introduced the `error`
keyword) and was present in v0.5.6 / v0.5.7 / v0.5.8 / v0.6.0 /
v0.6.1. None of those releases routed sub-process errors to the
DLQ correctly. Operators upgrading should expect their dispatcher-
style parsers (`switch ... default { error "..." }` with `process X`
in non-default arms) to start emitting DLQ entries that pre-fix
were silently absorbed; configure `control { error_log "..." }`
if you haven't already to capture them.

### Notes

- DSL syntax: unchanged.
- Public Rust API: unchanged. The fix is internal to `exec.rs`'s
  ProcessCall arm — no signature changes, no trait extensions.
- 361 tests pass (`cargo test --workspace`), `cargo build --release`
  green.
- Snippet library installation path: `/usr/share/limpid/snippets/`
  (the `_smoke-*.limpid` scaffolding under the repo root is the
  consumer-side `tail` config used to verify each parser locally;
  not packaged).
- Two regression tests added covering the sub-process error
  propagation contract: `test_exec_process_error_propagates_to_caller`
  (single-tier propagation) and `test_exec_try_catch_on_error`
  (try/catch still catches a sub-process Err post-fix).

---

## [0.6.1] - 2026-04-30
> perf: multi-pipeline scaling — 4-pipeline D-pipeline aggregate 374k → 459k events/sec (+23%, scaling 2.27× → 2.73×)

A short follow-up to v0.6.0 closing the multi-pipeline scaling gap
that the perf-milestone profile surfaced after release. Three small
changes that compound:

1. **Per-worker bump-arena recycling** — the per-event
   `bumpalo::Bump::new()` introduced in v0.6.0 became a contention
   point on the macOS xzm allocator's per-zone lock once multiple
   pipelines ran concurrently. Hoist the `Bump` into the per-input
   pipeline-worker task's local state and recycle via `Bump::reset()`
   between events. Steady state: zero allocations on the hot path.
2. **Pass the input event by reference through fan-out** — when
   multiple pipelines fan out from one input, the dispatcher used to
   `Event::clone()` per worker (workspace `HashMap` rebuild). The
   input event is read-only after `view_in` copies it into the
   per-event arena, so a `&Event` borrow is sufficient.
3. **`tracing/release_max_level_info`** — `trace!` / `debug!` macros
   compile to no-ops in release builds, eliminating per-event
   instrumentation cost (roughly half a percent of on-CPU on the
   multi-pipeline profile traced back to `mach_absolute_time` calls
   from tracing-event timestamps). Operators relying on `trace!` /
   `debug!` output need a debug build; `info!` / `warn!` / `error!`
   continue to fire.

### Changed — `pipeline::run_pipeline` signature

- New trailing parameter `bump: &mut bumpalo::Bump` — caller-supplied
  arena, reused across events. In-tree callers (`runtime`,
  `--test-pipeline` in `main`, unit tests) are migrated. Out-of-tree
  code that calls `run_pipeline` directly (rare; this is an internal
  API) passes `&mut bumpalo::Bump::new()`.
- `event` is now `&OwnedEvent` instead of `OwnedEvent`. Read-only
  access — `view_in` copies into the arena, the DLQ path constructs
  a fresh `OwnedEvent` from the borrowed view via `to_owned()`.

### Performance — single + multi pipeline (D pipeline, OCSF compose)

Same harness as v0.6.0. macOS, 16 physical cores. 3 reps each.

| Pipeline shape         | v0.5.7 | v0.6.0 | **v0.6.1** | Δ vs v0.6.0 |
|------------------------|-------:|-------:|-----------:|------------:|
| A passthrough          | 306k   | 303k   | **312k**   | +3%         |
| B `syslog.parse`       | 181k   | 282k   | **305k**   | +8%         |
| C parse + regex + if   | 73k    | 112k   | **115k**   | +3%         |
| D OCSF compose (UDP)   | 46.3k  | 168k   | **168k**   | ±0%         |
| D OCSF compose (TCP)   | n/a    | 170k   | **168k**   | ±0%         |
| **D 4-pipeline aggr.** | n/a    | 374k   | **459k**   | **+23%**    |

(eps/core for single-pipeline rows; eps aggregate for the
4-pipeline row. 4-pipeline is 4× independent inputs / pipelines /
outputs sharing one process.)

Scaling on the 4-pipeline configuration improves from 2.27× the
single-pipeline number on v0.6.0 to **2.73×** on v0.6.1.
Single-pipeline throughput is essentially unchanged — there's no
concurrency to expose the contention this patch removes, and the
remaining levers are noise-magnitude individually. The lift comes
when the daemon is actually running multiple pipelines, which is
the production deployment shape.

The remaining 4-pipeline gap to true linear scaling (~3.5–4× of
single-pipeline) is dominated by allocator activity in
`OwnedEvent::clone` and HashMap operations in workspace handling
that the per-event arena doesn't reach (event metadata between
input task and pipeline worker, queue boundaries, etc). Closing it
is a multi-day refactor — Linux native bench + `Arc<Event>` between
input and pipeline worker — and not in scope for this patch.

### Notes

- DSL surface, config surface, and CLI surface: unchanged.
- The `Output` plugin trait is unchanged; out-of-tree output sinks
  written against v0.6.0 work without modification.
- 384 tests pass. `cargo build / clippy --release` green.
- Operators with genuinely high pipeline counts (≥ 16) can still
  override the default tokio worker thread count via
  `TOKIO_WORKER_THREADS=…` if their workload benefits — this release
  does not cap it (an earlier draft did, and it backfired in benches
  that had > 8 active tokio tasks).

## [0.6.0] - 2026-04-30
> perf milestone — D pipeline 46.3k → 168k eps/core (+263%); per-event arena, direct serializer, key interning, `CompactString`, and the `Output` boundary refactor

The v0.6.0 release closes the perf milestone framed in the v0.5.7 →
v0.6.0 plan: collapse per-event allocation cost on the DSL hot path
to the point that real work (I/O + tokio scheduling + the actual
serializer) becomes the bottleneck. The headline number on the D
pipeline (OCSF Authentication compose + `to_json`) is **168k
eps/core**, up from 46.3k at v0.5.7 baseline — past the 100k
milestone target by 65%.

DSL-surface and config-surface compatibility: **unchanged**. Every
`def process / def pipeline / def input / def output` written
against v0.5.x continues to parse, type-check, and run. The breaking
changes in this release are confined to the **`Output` plugin
trait**; in-tree sinks (`file`, `tcp`, `udp`, `unix_socket`,
`stdout`, `http`, `otlp`, `kafka`) are migrated. Out-of-tree custom
output sinks need to migrate (see "Output trait — breaking change"
below).

### Performance — cumulative result

| Pipeline | DSL shape | v0.5.7 | **v0.6.0** | Δ |
|---|---|---:|---:|---:|
| A | passthrough | 306k | 303k | ±0% |
| B | `syslog.parse(ingress)` | 181k | 282k | +56% |
| C | parse + 2× regex + if/else | 73k | 112k | +54% |
| **D** | **OCSF compose + to_json** | **46.3k** | **168k** | **+263%** |

(eps/core, single-pipeline single-input, channel-direct injection,
UDP discard sink. 3 reps each, run-to-run spread ≤ 3.4%. Local
measurement; raw data is not committed to the repo.)

Flamegraph composition flipped vs v0.5.7 baseline:

| Category | v0.5.7 | **v0.6.0** |
|---|---:|---:|
| `malloc / free` | 42.99% | **14.93%** |
| `HashMap` / `IndexMap` rebuild | 11.77% | **4.00%** |
| `Clone` | 2.89% | **0.09%** |
| `__sendto` (output I/O) | n/a | 17.85% |
| tokio runtime | n/a | 10.40% |

`Value::to_owned_value`, `IndexMap::insert_full`, and the
`OwnedValue` `drop_in_place` chain — the top-three alloc-related
leaves at v0.5.7 — have all dropped out of the top 25 on v0.6.0.

### Added — bumpalo per-event arena (`crates/limpid/src/dsl/arena.rs`)

Every event entering `run_pipeline` gets a fresh
`EventArena<'bump>` whose lifetime ends when the event finishes
processing. All transient `Value::Object` / `Value::Array` /
`Value::String` / `Value::Bytes` payloads allocate from this arena;
the per-allocation `drop_in_place<Value>` chain (~23% of allocator
samples on the v0.5.7 D pipeline) collapses into a single
chunk-group free at event end.

The DSL `Value` enum is now lifetime-bound (`Value<'bump>`) —
internal API change for embedders and out-of-tree DSL extensions
(see "Out-of-tree extension migration" below). DSL configs are
unchanged.

### Added — direct `serde::Serialize for Value<'bump>`

`to_json(workspace.x)` and other JSON-emit paths previously routed
through an intermediate `serde_json::Value` tree. Implementing
`Serialize` directly on the arena-backed `Value` skips that copy,
collapsing `value_view_to_json` (1.11% of profile on the prior
revision) to zero.

### Added — static-literal key interning in DSL hashes

`HashLit` keys (the `metadata`, `actor`, `src_endpoint`, … leaves
of an OCSF compose) are interned at construction so the per-event
`arena.alloc_str(...)` cost runs once at registry-build time, not
once per event. This was the single largest unexpected win of the
milestone (+13% on D, ~3× the planned estimate).

### Added — `CompactString` for `OwnedValue::String`

Short owned strings (≤ 24 bytes — covers most metadata fields:
hostnames, IP strings, schema names, status enums) inline into the
enum payload, eliminating a heap allocation per leaf for the common
case. Long strings still spill to the heap unchanged.

### Changed — boundary refactor: `Output` trait split

**This is the only operator-visible breaking change in v0.6.0**, and
it only affects out-of-tree output sinks. In-tree sinks are migrated
in this release.

The pre-v0.6.0 `Output` trait took a fully-owned `&Event` at the
sink boundary, which forced `BorrowedEvent::to_owned()` on every
output statement — rebuilding the workspace HashMap (~10% on-CPU on
the prior profile).

The new shape:

```rust
#[async_trait]
pub trait Output: HasMetrics<Stats = OutputMetrics> + Send + Sync + 'static {
    /// Hot path: build a sink-specific payload from a borrowed event,
    /// using the per-event arena for any DSL eval (template paths,
    /// dynamic keys, etc.).
    fn render(
        &self,
        ev: &BorrowedEvent<'_>,
        arena: &EventArena<'_>,
    ) -> anyhow::Result<RenderedPayload>;

    /// Hot path: consume the rendered payload (downcast to the sink's
    /// concrete payload type) and perform I/O.
    async fn write(&self, payload: RenderedPayload) -> anyhow::Result<()>;

    /// Cold path (disk-queue replay): consume an `Event`. Default
    /// impl builds a transient arena, calls `view_in -> render ->
    /// write`. Sinks with a faster owned-form may override.
    async fn write_owned(&self, ev: &Event) -> anyhow::Result<()> { /* default */ }
}
```

`RenderedPayload` is a type-erased `Box<dyn Any + Send>` that each
sink defines a concrete payload struct for (`FilePayload`,
`UdpPayload`, …) and downcasts inside `write` — out-of-tree plugin
sinks remain fully extensible without changes to the core. `Module`
is no longer a supertrait of `Output` (`Module::from_properties` is
`Sized`-bound and would forbid `dyn Output`); construction sites
carry the `Module` bound separately.

`SinkInput { Owned, Rendered }` carries either form across
`QueueSender`. Memory queues flow `Rendered` (no `to_owned` cost on
the hot path); disk queues flow `Owned` only (Serialize/Deserialize
survives restart). `CompiledConfig` exposes `outputs_queue_kind` so
the pipeline executor routes at the output statement without
consulting runtime state.

Retry semantics: `Owned` retains the full N-attempt retry loop
(event is cloned up front); `Rendered` is single-shot (a
`Box<dyn Any>` is consumed on first `write`). Sinks needing full
retry should configure a disk queue. Documented at the
`write_with_retry` call site.

### Out-of-tree extension migration

If you maintain an out-of-tree DSL function or output sink, the
following internal API surfaces changed:

- **DSL functions** (in-tree primitives are migrated): the closure
  signature passed to `FunctionRegistry::register*` now takes
  `(arena, args, event)` (was `(args, event)`). `Value` is
  `Value<'bump>` and `Copy`. `FunctionRegistry::call` takes a
  `&BorrowedEvent<'bump>` and `&'bump EventArena<'bump>` in addition
  to the prior args.
- **Output sinks**: implement `render` / `write` / (optionally)
  `write_owned` per the trait shape above. `Module::from_properties`
  is unchanged for construction.
- **Custom processes**: `ProcessRegistry::call` takes
  `BorrowedEvent<'bump>` + `&'bump EventArena<'bump>` instead of an
  owned `Event`.

### Carried over from v0.5.8

The v0.5.8 release line is fully present in v0.6.0:

- `coalesce(a, b, c, ...)` first-non-null variadic primitive
- `syslog.parse` RFC 3164 TAG anchor fix (CEF inner-`": "` payload
  no longer absorbs into TAG/MSG split)
- `let f = <Object>; f.x.y` resolves through the local scope
  (read-side dot-access on let-bound Objects)

### Notes

- Build dependency: `bumpalo` (per-event arena), `compact_str`
  (small-string optimisation for owned values).
- Test count grew to 384 — coverage on the syslog/CEF parsers and
  `coalesce` was rebuilt from scratch for the new arena-shaped API
  (the v0.5.x pre-arena tests did not migrate cleanly).
- `--test-pipeline` / `--check` modes fall through to `SinkInput::Owned`
  when no live sinks are wired (no behavioural change for users).

## [0.5.8] - 2026-04-29
> `coalesce(...)` built-in for first-non-null fallback chains, plus a follow-up fix for dot-access on `let`-bound Object values

### Added — `coalesce(a, b, c, ...)` built-in (variadic)

A flat primitive that returns the leftmost non-null argument, or
`null` when every argument is null. Designed to replace the verbose
`switch true { x != null { x } default { y } }` pattern that snippet
composers had to repeat per OCSF leaf for the "use the parsed value
when present, fall back to an environment value otherwise" idiom:

```
// before — per leaf, 4 lines plus indentation:
let event_time = switch true {
    workspace.limpid.time != null { workspace.limpid.time }
    default { received_at }
}
// after:
let event_time = coalesce(workspace.limpid.time, received_at)
```

Semantics:

- accepts ≥ 1 argument; the analyzer rejects zero-arg calls and the
  runtime returns the same arity error
- all arguments are evaluated (DSL has no short-circuit at call
  sites); since DSL identifiers and built-ins are pure, eager
  evaluation has no observable difference from short-circuit
- only `null` is "passed over" — empty strings, zero, empty objects,
  and empty arrays are real present-but-empty values and are
  returned as-is. Callers who want "blank string is also absent"
  express that explicitly

Implementation note: this is the first variadic built-in. The
`Arity::Variadic { min }` enum variant was reintroduced (it had been
removed earlier as unused). Adding the variant is a non-breaking
extension — every existing built-in continues to use `Fixed` or
`Optional`. The analyzer's argument type-check uses the single
declared element type for every actual argument slot.

This is the fourth DSL gap surfaced and fixed mid-snippet-library
work — alongside `error` (v0.5.5), the `source` reshape (v0.5.6),
and `null_omit` (v0.5.7).

### Fixed — `let f = <Object>; f.x.y` resolves correctly

`let f = regex_parse(...); f.user` was failing at runtime with
`unknown identifier: f.user`. The local-scope path-resolver in
`crates/limpid/src/dsl/eval.rs` only consulted let bindings for
single-segment idents (`parts.len() == 1`), so any multi-segment
access whose root happened to be let-bound (`f.user`, `f.a.b`,
`f.list[0].kind`) skipped scope lookup entirely and fell through to
the catch-all "unknown identifier" arm. The analyzer's UnknownIdent
warning had the same gap.

The fix extends both code paths: when the first segment matches a
let binding, the runtime walks the bound value via the same
`resolve_workspace_path` Object/Array walker used for
`workspace.x.y.z`, and the analyzer suppresses the warning for the
whole path. Missing keys yield `Null` to match the workspace
path-walker contract — callers handle absence via `coalesce` or
explicit null comparison.

```
// before — runtime "unknown identifier: f.user":
def process parse_xxx {
    let f = regex_parse(workspace.body, "(?P<user>\\S+)")
    workspace.limpid = { user: f.user }     // ← runtime error
}
// after — works as written:
def process parse_xxx {
    let f = regex_parse(workspace.body, "(?P<user>\\S+)")
    workspace.limpid = { user: f.user }     // ✅ "alice"
}
```

Surfaced while writing parse_asa (Cisco ASA syslog parser) — every
per-message-ID leaf does `let f = regex_parse(workspace.asa.body,
"...")` and reads named captures via `f.user` / `f.src_ip` / etc.

Two regression tests added covering the happy path and the
missing-key (Null) path.

### Notes

- No DSL syntax change. `coalesce` is a regular flat primitive call.
  The let-bound dot-access fix is a behaviour change in path
  resolution semantics: before, `f.x` failed; after, it walks into
  the bound Object.
- No breaking changes (the only behaviour shift is the previously-
  failing case starting to work).

---

## [0.5.7] - 2026-04-29
> `null_omit` built-in to drop `null` keys from HashLit composer output

### Added — `null_omit(value)` built-in for HashLit cleanup

A flat primitive that recursively strips `null` from objects and
arrays. Designed for the OCSF-shape composer pattern (build a HashLit
from parser-populated workspace fields, then `to_json` for `egress`).
Without it, every absent field renders as `"key": null` in the output
— OCSF schema validation in Sentinel / Splunk DM often chokes on
that.

```
workspace.limpid = {
    class_uid: 4001,
    src_endpoint: { ip: workspace.cef.src, port: to_int(workspace.cef.spt) },
    dst_endpoint: workspace.cef.dst_endpoint,   // may be null on this event
    traffic: workspace.cef.traffic              // may be null on this event
}
egress = to_json(null_omit(workspace.limpid))
//  → {"class_uid":4001,"src_endpoint":{"ip":"...","port":...}}
//    (dst_endpoint and traffic dropped cleanly)
```

Semantics (recursive, single pass):

- `null` keys are dropped from objects (or top-level `null` returns
  `null`); the function recurses into the remaining values
- arrays are **not** compacted — a `null` slot in an array survives
  unchanged, because that's often the parser's placeholder ("this
  slot was unknown") and silently dropping it would hide the signal.
  The function recurses into non-null elements only. Use a dedicated
  array primitive when array compaction is the goal
- empty containers (`{}` / `[]`) are kept — the function strips
  `null` keys, it doesn't collapse a structure that just became empty
- scalars (`String`, `Int`, `Float`, `Bool`, `Bytes`, `Timestamp`)
  pass through unchanged

This is the third DSL gap surfaced and fixed mid-snippet-library
work — alongside `error` (v0.5.5) and the `source` reshape (v0.5.6).
The pattern is "implement broadly across vendors, surface DSL gaps,
fix in 0.5.x patches before locking 0.6.0", and it's working as
intended.

## [0.5.6] - 2026-04-27
> `source` reshaped to `{ip, port}` across DSL, wire, and tooling

### Changed (breaking) — `source` is now an Object with `.ip` and `.port`

The reserved DSL identifier `source` previously resolved to a flat
`String` containing only the peer IP. Starting in 0.5.6 it resolves
to an `Object { ip: String, port: Int }`, mirroring how `workspace`
is already structured. This unlocks two things the IP-only form
couldn't:

- Discriminating between two log originators bound to different
  source ports on the same host (a common multi-tenant pattern):
  `source.port == 5140` separates them.
- Faithful event capture for replay: a composer can write
  `${source.ip}:${source.port}` to produce a record `inject --json`
  accepts without losing the port to a `:0` placeholder.

```
// Before (≤ 0.5.5):
if source == "192.0.2.10" { drop }
output file { path "/var/log/${source}/events.log" }

// After (0.5.6+):
if source.ip == "192.0.2.10" { drop }
output file { path "/var/log/${source.ip}/events.log" }
```

Migration: every site that compares `source` to a String, interpolates
`${source}` into a path/template, or concatenates `source` with `+`
needs `.ip` appended. The analyzer surfaces the mismatch via the
existing type-check pass — bare `source` is now `Object`, and an
`Object == String` comparison or string-context interpolation flags as
a type warning.

### Changed (breaking) — wire format `source` matches the DSL shape

`tap --json`, `inject --json`, the error_log (DLQ), and the
`--test-pipeline --input` parser now emit and accept `source` as the
same `{ip, port}` object the DSL ident exposes:

```jsonc
// Before (≤ 0.5.5):
{ "source": "192.0.2.10:5140", ... }

// After (0.5.6+):
{ "source": { "ip": "192.0.2.10", "port": 5140 }, ... }
```

This eliminates the DSL/wire shape mismatch and lets a composer write
`source: source` to round-trip cleanly. JSONL files captured by
limpid 0.5.5 or earlier are no longer replayable on 0.5.6 without
preprocessing — operators with archived captures can convert with
`jq` (`'.source |= (split(":") | {ip:.[0], port:(.[1]|tonumber)})'`)
before piping into `inject --json`.

The breaking surface stays bounded: operator-facing DSL and the
JSONL wire shape are the only two places `source` is exposed.
Pre-1.0 lets us reshape both together while the snippet library is
still being authored, rather than later when external configs and
captures depend on the old form.

## [0.5.5] - 2026-04-27
> `error` routing keyword for explicit DLQ routing

### Added — `error` routing keyword for explicit DLQ routing

Process and pipeline bodies now accept an `error` statement alongside
`drop` and `finish`:

```
def process parse_fortigate_cef {
    workspace.cef = cef.parse(workspace.syslog.msg)
    switch workspace.cef.name {
        "traffic" { process parse_fortigate_cef_traffic }
        "utm"     { process parse_fortigate_cef_utm }
        default   { error "unsupported FortiGate CEF subtype: ${workspace.cef.name}" }
    }
}
```

`error` takes an optional message expression — anything an `${...}`
template can render — and routes the event to the [error log](./operations/error-log.md)
exactly like a runtime process failure: counted as `events_errored`,
written to `control { error_log "..." }` if configured, otherwise
emitted as a structured `tracing::error!` line. The message lands in
the DLQ entry's `reason` field so the operator sees *why* an event was
rejected without reverse-engineering the bytes.

This fills a gap that snippet libraries hit immediately: a parser
dispatcher that can't recognise the input subtype previously had to
choose between `drop` (silent loss, looks intentional) and a
hand-rolled runtime panic. Neither matches the intent of "this event
was supposed to be processable but I cannot — operator action needed."
`error` makes that intent first-class.

The keyword is rejected inside `def function` bodies (function body
grammar is `let* + trailing expression`, no statement forms allowed) —
pure expression functions stay pure.

## [0.5.4] - 2026-04-27
> User-defined pure functions (`def function`) with let-form bodies

### Added — `def function` for pure expression functions

User-defined functions are now a top-level definition kind, alongside
`def input` / `def output` / `def process` / `def pipeline`. The body
is zero or more `let` bindings followed by a required trailing
expression that becomes the return value. Designed for the small
mapping / lookup helpers that vendor parsers reuse — protocol number
→ name, severity string → OCSF `severity_id`, action string →
activity_id — and for the small chains of intermediate values that
make those mappings readable.

```
def function normalize_proto(num) {
    switch num {
        6  { "tcp" }
        17 { "udp" }
        1  { "icmp" }
        default { null }
    }
}

def function severity_id_from_label(s) {
    let lowered = lower(trim(s))
    switch lowered {
        "critical" { 5 }
        "high"     { 4 }
        "medium"   { 3 }
        "low"      { 2 }
        "info"     { 1 }
        default    { 1 }
    }
}

def process parse_fortigate_cef_traffic {
    workspace.limpid = {
        connection_info: {
            protocol_num:  workspace.cef.proto,
            protocol_name: normalize_proto(workspace.cef.proto)
        },
        severity_id: severity_id_from_label(workspace.cef.severity),
        ...
    }
}
```

User-defined functions register into the same `FunctionRegistry` as
built-in primitives — call sites dispatch through the standard
`(namespace, name)` lookup, the analyzer arity-checks them the same
way, and they compose anywhere an expression goes (HashLit values,
function arguments, binary operands, output templates, pipeline-level
`if` conditions). Function names must be bare identifiers; the dot
namespace is reserved for schema-bound built-ins.

`let` is the assignment form for local-scope variables in the body —
each `let x = …` line binds (or reassigns) `x` in the same scope.
Re-binding the same name simply overwrites the prior value; there is
no separate declaration step, no `let mut`, and no `x = …`
re-assignment syntax. Each let RHS sees parameters and earlier lets;
the trailing expression sees everything.

To keep functions pure, the analyzer rejects function bodies that:

- read from the Event (`ingress`, `egress`, `source`, `received_at`,
  `error`, any `workspace.*` path) — anywhere in the body, including
  inside a `let` RHS;
- reference a free variable that's neither a parameter nor an
  Event-bound name (a `config.foo` or bare `result` typo surfaces at
  `--check` time instead of failing at runtime);
- call into a user-defined `def process` (process bodies have side
  effects functions can't tolerate); or
- participate in a function-to-function call cycle (direct
  self-recursion or mutual recursion through a chain). If recursion
  is genuinely needed, use `def process` instead.

All four are hard errors at `--check` time — the config fails to load
and the daemon won't start until they're fixed.

Side effects (`workspace.x = …`, `egress = …`, `drop` / `finish` /
`output` routing, statement-form `if` / `switch` / `foreach`
/ `try-catch`) are rejected at the parser level — function body
grammar accepts only `let` bindings and a trailing expression, so
those statement forms simply aren't in the grammar.

A new expression-form `switch` lands at the same time. Each arm
body is one expression; the matching arm's value is the value of
the whole `switch`. Distinct from the statement-form `switch` in
process / pipeline bodies (which routes events / mutates
workspace). Use the expression form inside `def function` bodies,
inside `let` RHS, or anywhere a value is expected.

## [0.5.3] - 2026-04-27
> limpidctl stats surfaces errored counters

### Fixed — `limpidctl stats` shows `events_errored` / `events_errored_unwritable`

The 0.5.2 pipeline metrics gained `events_errored` and
`events_errored_unwritable` but the human-readable `limpidctl stats`
renderer wasn't updated — the JSON form (`limpidctl stats --json`,
control socket, Prometheus) carried both, the default text form
silently dropped them. Operators saw zero on `stats` while the
real number was hiding in the JSON.

The columns now render when they're non-zero:

```
Pipelines:
  ama_forward         89 received  35 finished  23 dropped   0 discarded  31 errored
  splunk_archive      62 received  38 finished  24 dropped   0 discarded
```

Steady-state pipelines (no errors) keep the compact row — a column
of zeros across every pipeline in the common case is just noise. A
non-zero `events_errored_unwritable` adds a second column on top of
`errored`.

## [0.5.2] - 2026-04-27
> Dead-letter queue for process errors

### Changed — process runtime errors route to a dead-letter queue (revising 0.5.1)

0.5.1 changed the pipeline so that a `process` runtime error caused
the event to be **discarded** with a counter increment. That was
appropriate for surfacing the silent corruption that 0.5.0's
"warn-and-continue" produced, but for a log pipeline default-discard
is itself a strong failure mode — security telemetry should not lose
events to a config bug at the receiving SIEM.

The 0.5.2 default sets the failed event aside in a **dead-letter
queue** (DLQ) so the operator can audit, fix the offending config,
and replay:

- New `control { error_log "/var/log/limpid/errored.jsonl" }`
  property opts in to a JSONL file. Each errored event becomes one
  line:

  ```json
  {
    "timestamp": "...",
    "reason": "...",
    "process": "wrap_journal",
    "pipeline": "journal_forward",
    "event": {"source": "...", "received_at": ..., "ingress": "..."}
  }
  ```

  The `event` sub-object is exactly what `limpidctl inject --json`
  needs to reconstruct a fresh Event, so replay is:

  ```bash
  jq -c '.event' /var/log/limpid/errored.jsonl \
      | limpidctl inject input <name> --json
  ```

- When `error_log` is **unset**, the same record is emitted as a
  structured `tracing::error!` line so the data is never silently
  lost — it just lives in journald / stderr instead of a dedicated
  file. Operators using the daemon under systemd can still recover
  via `journalctl -u limpid -o json | jq …`.

- New `events_errored_unwritable` counter (and
  `limpid_pipeline_events_errored_unwritable_total` Prometheus
  metric): subset of `events_errored` for which the DLQ write itself
  failed (disk full, permissions, rotation race). The runtime falls
  back to the tracing channel; alarm on this counter — non-zero
  means the replay path may be incomplete.

- The pipeline-runtime trace now reads `event → error_log` instead
  of `event discarded`. `--test-pipeline` prints the would-be JSONL
  record after the trace so operators can rehearse the replay
  recipe without booting the daemon.

The downstream behaviour is unchanged from 0.5.1: errored events
still don't reach any output, so there is no shape regression in the
production stream. What changes is that the events are now
**recoverable**.

### Fixed — DLQ writer hardening (audit follow-up)

- **Concurrent line interleave**: multiple pipeline workers calling
  `ErrorLogWriter::write` no longer race. POSIX `O_APPEND` atomicity
  only covers writes ≤ `PIPE_BUF` (Linux: 4 KiB), and DLQ records
  carrying base64-encoded binary `ingress` easily exceed that. An
  in-process `tokio::sync::Mutex` serialises the open + write
  sequence so each JSONL line is written whole.
- **Startup path validation**: `error_log` parent directory is
  stat()'d at daemon start; a typo'd / missing path is rejected
  before any event reaches the failure path. Previously the typo
  surfaced as `events_errored_unwritable` ticks at first failure.
- **Rotation guidance**: `operations/error-log.md` now ships a
  recommended `logrotate` configuration (`copytruncate` + `maxsize
  1G`) so the DLQ has a documented disk-fill ceiling. In-process
  rotation is deferred to v0.6.0; operator-side `logrotate` covers
  the realistic blast radius for v0.5.2.

## [0.5.1] - 2026-04-27
> Analyzer strictness + pipeline error handling

### Breaking — process runtime errors discard the event

When a `process` statement raises a runtime error (unknown identifier,
type mismatch, regex compile failure, …) the pipeline now **discards**
the event and increments a new `events_errored` counter, instead of
emitting a `WARN` and forwarding the event with its original `ingress`
unchanged.

The previous fallback ("warn-and-pass-through") combined poorly with
the analyzer gap that let unresolved bare identifiers slip past
`--check`: a config that referenced a renamed Event field
(e.g. pre-0.5 bare `timestamp`) loaded fine, then failed every event
at runtime — but the original ingress was forwarded downstream, so
the operator's wrap / enrichment process was silently bypassed.

Operators now see the failure in `events_errored` (and via the new
`limpid_pipeline_events_errored_total` Prometheus metric / per-trace
`error: ... (event discarded)` line), rather than discovering it
hours later at the receiving SIEM. Configs that intend partial
processing should use `try { ... } catch { ... }` to express that
intent explicitly.

The same routing applies to inline `process { ... }` bodies, which
previously bubbled the error up to the runtime as a Result and lost
the event without incrementing any pipeline counter.

### Added — analyzer flags unknown bare identifiers

`--check` now warns when a `process` body or expression references an
identifier that doesn't resolve to a reserved event ident
(`ingress`, `egress`, `source`, `received_at`, `error`), a `let`
binding, or a `workspace.*` path. The warning carries `DiagKind::UnknownIdent`
so `--ultra-strict` promotes it to an error in CI.

A bare `timestamp` reference — the most common 0.4→0.5 migration miss
— gets a targeted help line pointing at both alternatives:
`received_at` for the wall-clock event time, `timestamp()` for the
current instant. Other unknown idents fall back to the levenshtein
suggestion engine ("did you mean `ingress`?").

The `type` property of an `output` block (its bare-ident value is a
module-name reference resolved at config-load time, not a runtime
expression) is exempt — flagging `stdout`, `tcp`, etc. as unknown
would be a false positive.

## [0.5.0] - 2026-04-26
> OTLP transport + DSL surface freeze

### Changed — design principles restructured (still five)

The five design principles have been reorganised so each one carries
its own architectural weight, rather than mixing principles with
operating rules. The renumbered set:

1. **Zero hidden behavior** *(unchanged)*
2. **I/O is dumb transport** *(unchanged)*
3. **Only `egress` crosses hop boundaries** *(was Principle 4)*
4. **Atomic events through the pipeline** *(new)* — formalises the
   invariant that the pipeline never operates on bundles or fans out:
   inputs split wire-level batches into atomic Events, process snippets
   are 1-in-1-out (or 0 via `drop` / `finish`), outputs rebundle at the
   emit boundary. The OTLP envelope split, the `syslog_*` line split,
   the `batch_level` mode on the OTLP output — all are this one
   principle in different transports.
5. **Safety and operational transparency** *(new)* — formalises the
   software-construction stance that surfaces in every limpid feature:
   `--check` static analysis, `tap`/`inject`/`--test-pipeline` for
   verify-and-replay, `SIGHUP` atomic reload with rollback, retry +
   secondary + disk-WAL on outputs, `Drop` hooks for shutdown
   visibility. Principle 1 covers config-time transparency; Principle
   5 covers runtime transparency and recoverability.

What used to be Principles 3 (domain knowledge in DSL) and 5 (schema
identity by namespace) are now under a new *Operating rules* section
in the same document — they are concrete consequences of Principles 1
and 2 rather than independent architectural commitments. Anything
that previously cited *"per Principle 3"* should now cite *"per the
Domain knowledge in DSL operating rule"* or, more usefully, the
Principle the rule is derived from.

This is a docs-only change in v0.5.0; no code is affected. Pre-1.0,
this kind of clarification is expected.

### Added — OpenTelemetry Protocol (OTLP) support

OTLP becomes a first-class transport across both ingest and emit, with
all three OTLP wire formats supported:

- **Inputs**: [`otlp_http`](docs/src/inputs/otlp-http.md) (`POST /v1/logs`,
  `application/x-protobuf` and `application/json`) and
  [`otlp_grpc`](docs/src/inputs/otlp-grpc.md) (`opentelemetry.proto.collector.logs.v1.LogsService.Export`).
  Each LogRecord becomes one Event with `ingress` set to a singleton
  ResourceLogs (1 Resource + 1 Scope + 1 LogRecord), preserving full
  upstream context per Principle 2.
- **Output**: [`otlp`](docs/src/outputs/otlp.md) with
  `protocol "http_json" | "http_protobuf" | "grpc"`, `batch_size`,
  `batch_timeout`, `headers {}`, and TLS via system roots / custom CA.
- **Primitives** (in the new `otlp.*` namespace):
  `otlp.encode_resourcelog_protobuf` /
  `otlp.decode_resourcelog_protobuf` /
  `otlp.encode_resourcelog_json` /
  `otlp.decode_resourcelog_json`. HashLit shape mirrors the proto3
  tree with snake_case keys; JSON form applies the canonical OTLP/JSON
  conventions (camelCase, u64-as-string, bytes-as-hex).

The hop contract is "egress = singleton ResourceLogs proto bytes":
the process layer owns semantic conversion (severity mapping,
OCSF→OTLP shape) via DSL snippets; Rust ships only the mechanical
wire encode / decode (Principle 3).

### Added — OTLP throughput controls

Four orthogonal defense / throughput layers on the OTLP/HTTP input,
each opt-in (default unlimited) so existing configs are unaffected:

- **`body_limit`** *(default `16MB`)* — bytes per request. Larger
  bodies are rejected with HTTP 413 *Payload Too Large* before any
  decode work runs. axum's `DefaultBodyLimit` shows up in the layer
  chain, replacing axum's own 2 MiB default which is too small for
  collector-to-collector batches.
- **`max_concurrent_requests`** — in-flight request cap (semaphore).
  Worst-case decode memory becomes
  `max_concurrent_requests × body_limit`, turning the open-ended
  decode-amplification path into a known quantity. Excess requests
  fail-fast with HTTP 503 *Service Unavailable* (OTLP senders retry,
  so backpressuring the socket would amplify overload).
- **`request_rate_limit`** — sustained req/sec (token bucket, reuses
  the existing `RateLimiter`). Smooths burst above the configured
  rate; pairs with the concurrency cap because a token bucket allows
  full burst-equal-to-rate at idle.
- **`rate_limit`** — sustained events/sec, per-emitted-LogRecord. Same
  implementation as `syslog_*`, applied after request decode and
  split, so it caps pipeline-send rate independent of how the events
  arrived.

`otlp_grpc` gets `rate_limit` on the same axis. Per-RPC throttling
on the gRPC side relies on tonic's HTTP/2 stream limits and the
existing `rate_limit` after split — no new property.

### Added — `otlp_grpc` server-side TLS / mTLS

Optional `tls { cert key ca }` block on the input. With `cert` + `key`
the server presents a certificate; adding `ca` switches into mutual
TLS mode where every client must present a certificate signed by that
CA root. Mirrors the same block shape as `syslog_tls` (now parsed via
a shared `TlsConfig::from_properties_block` helper). PEM files are
loaded via `spawn_blocking` so a slow disk does not stall the tokio
reactor at startup.

For the output, gRPC client-side TLS already shipped in the initial
OTLP push; this release closes the symmetric server-side gap.

### Added — `otlp` output `batch_level` merging

Three settings, all producing OTLP that is semantically identical at
the receiver — they differ only in wire framing and CPU/wire-size
trade-off:

- **`none`** *(default)* — one ResourceLogs entry per buffered Event.
  Cheapest CPU, largest wire. Suitable when `batch_size = 1` or the
  collector tolerates redundancy.
- **`resource`** — Events sharing a Resource collapse into a single
  ResourceLogs entry; their ScopeLogs sit side-by-side under it.
- **`scope`** — as `resource` plus Events sharing a Scope inside the
  same Resource collapse into a single ScopeLogs whose
  `log_records[]` accumulates everything. Smallest wire, slightly
  higher CPU (Resource and Scope equality scans).

Resource and Scope equality is order-insensitive on attribute lists
because proto3 makes no canonical-order promise on the wire.

### Added — `otlp` output retry with exponential backoff

`retry { max_attempts initial_wait max_wait backoff }` block on the
output, parsed via the same `RetryConfig` shared with the file / tcp
/ http outputs. Internal retry is necessary specifically for the OTLP
output because it batches Events from multiple `write()` calls into
one request — without an internal retry, a single transient ship
failure would lose the entire drained batch (the queue layer's
per-event retry only re-pushes the most recent Event). Exhausted
retries bubble the error up so the queue's secondary / drop policy
still applies. Doubling under exponential backoff is `saturating_mul`
for explicit overflow safety.

### Added — `Value::Bytes` variant in the DSL

The DSL runtime value type gains a first-class `Bytes(bytes::Bytes)`
arm, replacing the `serde_json::Value`-based representation that
silently corrupted non-UTF-8 byte streams via `from_utf8_lossy` /
`String::into_bytes()`. User-facing surface is preserved:

- DSL syntax / semantics unchanged.
- `ingress` / `egress` reads return `Value::String` for UTF-8-clean
  data (the historical case) and only switch to `Value::Bytes` for
  non-UTF-8 content (which the previous code was already mangling).
- Existing primitives keep their return shapes.
- `tap --json` / persistence still emit JSON; `Value::Bytes` is
  encoded as `{"$bytes_b64": "..."}` with `$`-prefix key escaping
  for round-trip safety. The marker is internal; `to_json` /
  `parse_json` reject it.

Cross-primitive Bytes rules: text-only primitives (`upper`, `lower`,
`regex_*`, `contains`, `format`, `to_int`, `to_json`, template
interpolation, property traversal) error on Bytes — the
"no-implicit-coercion" rule. Hash primitives (`md5`/`sha1`/`sha256`) and
`len` accept Bytes natively. `Bytes + Bytes` concatenates byte-wise.

New conversion primitives at the text/binary boundary:
- **`to_bytes(s, encoding="utf8")`** — `utf8` (default) / `hex` / `base64`.
- **`to_string(b, encoding="utf8", strict=true)`** — `utf8` strict (errors
  on invalid UTF-8) or lossy, plus `hex` / `base64` printable forms.

### Breaking — `Event.timestamp` renamed to `Event.received_at`

The `Event` struct field, the reserved DSL identifier, the `format()`
template placeholder, and the JSON serialisation key are all renamed
from `timestamp` to `received_at`. The semantic clarification is that
this field is **strictly the wall-clock time at which this hop received
the event** — input modules never overwrite it from payload contents
(Principle 2: input is dumb transport). Source-claimed event times,
when extractable from the wire, surface in workspace fields like
`syslog_timestamp` / `cef_rt` / `pan_generated_time` via parser
primitives.

The old name was generic enough that some snippets and configs were
treating it as if it carried the source-claimed event time, which it
never reliably does.

**Migration** (mechanical sed across configs and any captured `tap --json`
files):

```sh
find /etc/limpid -name '*.limpid' -exec sed -i \
    -e 's/\${timestamp}/\${received_at}/g' \
    -e 's/%{timestamp}/%{received_at}/g' \
    -e 's/strftime(timestamp,/strftime(received_at,/g' \
    {} +

# Captured tap --json files: rewrite the top-level key
jq -c '.received_at = .timestamp | del(.timestamp)' \
    old-capture.jsonl > new-capture.jsonl
```

There is no deprecation alias — `${timestamp}` and `%{timestamp}` are
hard errors (analyzer / runtime) on v0.5.0+. The 0.5.0 release window
is the right moment for the cut because pre-1.0 breaking changes are
still expected.

### Breaking — schema parsers no longer prefix workspace keys

`syslog.parse` and `cef.parse` previously emitted keys with a
`<schema>_` prefix (`syslog_hostname`, `cef_name`, …) on the rationale
that workspace dumps would stay self-describing when several parsers
populated the same event. In practice the prefix collided with the
*capture* idiom — `workspace.s = syslog.parse(ingress)` produced
`workspace.s.syslog_hostname`, double-prefixed — and made schema
parsers behave inconsistently with format primitives (`parse_json`,
`parse_kv`) which always emit raw keys.

Both schema parsers now return un-prefixed keys (`hostname`,
`appname`, `version`, `name`, …). Namespacing is the operator's job
and is the recommended pattern:

```limpid
workspace.syslog = syslog.parse(ingress)   // workspace.syslog.hostname, ...
workspace.cef    = cef.parse(ingress)      // workspace.cef.version, workspace.cef.src, ...
```

Bare invocation still works (`syslog.parse(ingress)` merges keys flat
into `workspace`) but is collision-prone and discouraged. CEF
extension keys (`src`, `dst`, `act`, …) were never prefixed — those
names are part of the CEF spec and continue verbatim.

**Migration**: rewrite any references to `workspace.syslog_*` /
`workspace.cef_*` in configs and snippets. The capture form is
mechanically equivalent and clearer:

```sh
# 1. capture once at the top of each process body:
#      workspace.syslog = syslog.parse(ingress)
#      workspace.cef    = cef.parse(ingress)
# 2. rewrite the references:
sed -i 's/workspace\.syslog_/workspace.syslog./g; s/workspace\.cef_/workspace.cef./g' \
    /etc/limpid/**/*.limpid
```

### Breaking — `cef.parse` requires `CEF:` at position 0

Previously `cef.parse` located `CEF:` anywhere in the input (via
`find`) so a `<PRI>` syslog wrapper was silently skipped. This
overlapped responsibilities — header stripping is syslog's job, not
CEF's — and could match the literal string `CEF:` if it appeared
elsewhere in the payload.

`cef.parse` now requires the input to start with `CEF:`, erroring
with `cef.parse(): input does not start with \`CEF:\`` otherwise.
The canonical pattern when CEF is transported over syslog is:

```limpid
workspace.syslog = syslog.parse(ingress)
workspace.cef    = cef.parse(workspace.syslog.msg)
```

CEF arriving on transports without a syslog wrapper (HTTP, file
tail, …) is unaffected — `CEF:` is at position 0 already.

### Breaking — `syslog.parse` PRI parsing aligned with RFC 5424 §6.2.1

`syslog.parse` now validates the leading `<PRI>` header strictly: 1–3
ASCII digits, value 0–191, framed by `<` and `>` at the start of the
input. Inputs the previous parser tolerated silently — `<malformed
text>...` (non-digit content), `<999>...` (out-of-range), `<>...`
(empty PRI) — now error with `syslog.parse(): no PRI header`,
matching the behaviour of the sibling `syslog.strip_pri` /
`syslog.set_pri` / `syslog.extract_pri` primitives which already used
the strict scanner.

If you have a flow that depended on the old lax behaviour to ingest
non-syslog payloads via `syslog.parse`, switch to a different parser
(`parse_kv`, `regex_parse`, or a snippet) — calling `syslog.parse` on
something that isn't syslog has no defined output anyway.

### Added — `syslog.parse` emits `pri`, `facility`, `severity`, `timestamp`

Beyond the structural fields, `syslog.parse` now returns:

- **`pri`** (Int, 0–191) — the raw `<PRI>` value
- **`facility`** (Int, 0–23) — `pri / 8`
- **`severity`** (Int, 0–7) — `pri % 8`
- **`timestamp`** (String) — the source-claimed wire timestamp from
  the RFC 5424 / RFC 3164 header (previously dropped silently)

`pri` / `facility` / `severity` are always present (the parser errors
when no valid PRI is found, per the breaking change above). The
timestamp surfaces source-claimed event time for snippets that need
it — e.g. for the OCSF `time` field or the OTLP `time_unix_nano` —
without forcing a separate `extract_pri` + parse pass. The lighter
`syslog.extract_pri` is still available for callers that only need
the PRI byte without tokenising the rest of the header.

### Breaking — `output file` path templates are stricter

The `path` template renderer in the `file` output gained four guards
that reject configs the previous lax renderer accepted silently. Each
fires before any byte hits disk, per Principle 1 (zero hidden
behaviour).

- **Per-interpolation slash strip.** Every `${...}` result has
  forward and back slashes replaced with `_`, so an interpolation
  cannot smuggle a path separator into the rendered path. The
  invariant is "one interpolation = one path component"; directory
  structure has to live in the literal parts of the template.
- **`..` rejected anywhere in the rendered path.** After all
  interpolations resolve, the path is split on `/` and any component
  exactly equal to `..` causes the write to error rather than being
  silently rewritten.
- **Empty interpolation rejected.** An interpolation that evaluates
  to the empty string errors instead of producing surprise paths
  like `/foo//bar` or `/foo/.log`.
- **Trailing-slash / no-filename rejected.** A rendered path that
  ends in `/` (no filename component) errors before the auto-mkdir
  runs, so a stray template like `/var/log/${workspace.host}/`
  cannot create empty directories silently.

Configs that depended on any of these silent rewrites should
sanitise the inputs upstream (`regex_replace`, explicit fallbacks in
a `process` block) and reference the cleaned workspace key from the
template. Worked examples are in the
[`output file`](docs/src/outputs/file.md) reference.

### Breaking — `format()` primitive removed

The `format(template)` primitive — which expanded `%{...}` placeholders against the current event — has been removed. The `${expr}` interpolation that any string literal supports is strictly more capable: it accepts any DSL expression rather than the limited `%{event.x}` / `%{workspace.x}` set, and it's resolved at parse time so typos are caught by `--check`.

**Migration**: rewrite `format("...")` calls to interpolated string literals.

```limpid
// before
egress = format("[%{source}] %{workspace.cef_name}: %{egress}")

// after
egress = "[${source}] ${workspace.cef.name}: ${egress}"
```

The `%{...}` syntax is gone entirely; `${expr}` is the single template form.

### Breaking — `to_json()` requires an argument

`to_json()` (no argument) used to serialise the entire `Event` (received_at + source + ingress + egress + workspace) as JSON — the same shape as `tap --json`. In practice operators almost always wanted the workspace alone (the parsed/enriched form to ship downstream), so the no-arg default was a hidden footgun.

`to_json` now requires exactly one argument. The most common pattern:

```limpid
egress = to_json(workspace)
```

For the old whole-event behaviour, build the shape explicitly: `to_json({received_at: received_at, source: source, workspace: workspace})`.

### Added — `parse_kv` separator argument

`parse_kv(text, separator)` lets the caller pass a single-byte
separator (default `' '`). Comma-separated KV payloads — common in
Cisco ASA, Microsoft Defender, and various OEM telemetry — now
parse without a regex pre-pass:

```limpid
workspace.kv = parse_kv(workspace.syslog.msg, ",")
// "a=1,b=2,c=\"three,four\"" → {a: "1", b: "2", c: "three,four"}
```

Quoted values still work and may contain the separator (e.g. a comma
inside a quoted string when separator is comma). The defaults hash
literal can sit either as the second argument (when separator is the
default space) or as the third (after an explicit separator).

### Breaking / Added — `Value::Timestamp` first-class DSL type

The DSL gains a typed `Value::Timestamp(DateTime<Utc>)` value arm.
Inputs in any timezone (RFC3339 with offset, naive + explicit `tz`
argument, etc.) are normalised to UTC at the boundary, so the
runtime never has to reason about mixed offsets.

Previously every timestamp travelled through the runtime as an
RFC3339 `Value::String` — type-unsafe, repeated parse cost, and easy
to typo into `contains(received_at, "2026")` (silently false because
of substring semantics).

Now:

- **`received_at`** → `Value::Timestamp` (was `Value::String`)
- **`timestamp()`** (new, replaces `now()`) → `Value::Timestamp`
- **`strptime(value, fmt[, tz])`** → `Value::Timestamp` (was String)
- **`strftime(timestamp, fmt[, tz])`** — first argument must be a
  `Value::Timestamp` (was String, parsed RFC3339 internally).
  Passing a string is a clear type error: `strftime(): first argument
  must be a timestamp, got string`.
- **`to_int(timestamp)`** → unix nanoseconds (`i64`), matching OTLP
  `time_unix_nano`. So `to_int(received_at)` is the natural way to
  get an epoch-nanos number.
- **String coercion** of `Value::Timestamp` (e.g. `${received_at}`,
  `to_string()`-style paths) renders RFC3339 — the user-visible
  surface is unchanged from 0.4 for type-correct configs.

DSL syntax does **not** change. Existing type-correct expressions
(`strftime(received_at, "%Y-%m-%d", "local")`, `${received_at}`) keep
working byte-for-byte. Only code that round-tripped timestamps
through string operations (`contains(received_at, "...")`,
`len(received_at)`, regex on `received_at`) errors at the analyzer or
runtime — those were always meaningless on a timestamp and now fail
loudly.

`now()` is removed; rename call sites to `timestamp()`. The new name
matches the value type it returns and reads consistently with
`received_at`.

### Breaking — `tap --json` and `inject --json` use unix nanoseconds for `received_at`

`tap --json` previously emitted `received_at` as an RFC3339 string;
it now emits an `i64` of unix nanoseconds, matching OTLP
`time_unix_nano`. `inject --json` reads the same wire form.
Pre-0.5 captures (`*.jsonl` files holding RFC3339 strings) need to
be migrated before replay:

```bash
jq -c '.received_at = (.received_at | sub("\\.\\d+"; "") | strptime("%Y-%m-%dT%H:%M:%S%z") | mktime * 1000000000)' \
    old-capture.jsonl > new-capture.jsonl
```

(For sub-second precision use a real script — `jq` doesn't carry
nanos. The simpler migration is to discard old captures; nothing
about pipeline correctness depends on replaying historical traffic
through the new format.)

### Added — host / version primitives

- **`hostname()`** → `String` — the local machine's hostname, resolved at every call via `gethostname(2)`. Useful for tagging events with the forwarder's identity (`workspace.forwarded_by = hostname()`) and populating OTLP `host.name` resource attributes.
- **`version()`** → `String` — the limpid daemon's version baked in at compile time (e.g. `"0.5.0"`). Useful for provenance markers and OTLP `service.version`.

`hostname()` was previously referenced in the OTLP example block in the docs but was not actually implemented — that drift is closed.

### Added — `starts_with` / `ends_with` string predicates

Two new flat primitives complement `contains`:

- **`starts_with(haystack, needle)`** — `true` if `haystack` begins with `needle`.
- **`ends_with(haystack, needle)`** — `true` if `haystack` ends with `needle`.

Use these when *position* matters — e.g. dispatching to the right
parser based on a leading prefix (`starts_with(workspace.syslog.msg,
"CEF:")`) — rather than `contains`, which matches anywhere and would
fire on a literal `CEF:` string buried elsewhere in the payload.

### Added — DSL primitives

- **`to_int(x)`** — coerce a value to `i64` (strings, floats, bools, nulls);
  returns `null` on unparseable input. Primary use: casting CEF extension
  values and CSV column strings to numeric OCSF fields (ports, session IDs).
- **`find_by(array, key, value)`** — locate the first object in an array
  whose `key` field equals `value`. No type coercion; `null` on no match.
  Designed for identity-based access to schemas that ship arrays-of-objects
  (MDE evidence, OCSF observables).
- **`csv_parse(text, field_names)`** — parse a single CSV row into an object
  keyed by the supplied field names, with RFC 4180 quoting. Replaces the
  `regex_parse` workaround for vendors (most notably Palo Alto) that emit
  100+-field positional CSV syslog records.
- **`len(x)`** — cardinality for `Array` (elements), `String` (Unicode
  characters), `Object` (top-level keys). Scalars return `null`.
- **`append(arr, v)` / `prepend(arr, v)`** — return a new array with `v`
  added at the back / front. Input is unchanged; callers re-bind.

### Added — DSL arrays (positionless collections)

- **Array literals** (`[a, b, c]`, `[]`, mixed types, nesting, trailing
  commas) are now first-class expressions, evaluating to `Value::Array`
  at runtime. Grammar, AST (`ExprKind::ArrayLit`), parser, evaluator, and
  analyzer (`FieldType::Array`) all updated.
- **No positional access.** `arr[n]` and `arr[n] = v` are intentionally
  absent from the grammar. Arrays are addressed by identity (`find_by`,
  `foreach`) and mutated by "back / front" semantics (`append`,
  `prepend`). Numeric indexing drifts under insert / delete; identity
  addressing survives. See
  `docs/src/processing/user-defined.md#arrays` for the rationale.

### Fixed — security hardening from the v0.5.0 audit

- **OTLP output: header values no longer logged on validation failure.**
  The configured `headers { ... }` block typically holds bearer tokens
  / API keys. Previously, a malformed value would produce a
  `tracing::warn!` containing both key and value verbatim — leaking
  the credential into the log stream on misconfiguration. Now logs
  the key only, with explicit `value redacted`.
- **OTLP output: graceful-shutdown buffer warning.** `OtlpOutput`
  gained the `Drop` impl that `HttpOutput` already had: aborts the
  pending deferred-flush task and warns operators about events still
  in the buffer at shutdown. The events are not actually lost (the
  queue layer re-delivers from spool), but the count is now visible.
- **OTLP/HTTP: bounded decode-error log line.** `serde_json` /
  `prost` error wording is capped at 256 characters in the warn log
  to remove a pathological-payload log-amplification primitive.
- **OTLP gRPC input: panic-free peer fallback.** The `remote_addr()`
  fallback for non-TCP transports now constructs the unspecified
  `SocketAddr` directly instead of parsing a constant — removes a
  panic seed that any future refactor of the literal could revive.
- **OTLP output retry: saturating doubling.** `wait * 2` under
  exponential backoff is `saturating_mul(2)`. The realistic reach of
  `Duration` overflow is "never" (~584 years) but the explicit bound
  removes another panic seed.
- **`hostname()` panic-safe.** The `gethostname` 0.5.x crate panics
  on `gethostname(2)` syscall failure (chroot / namespace edge
  cases — vanishingly rare in practice). The primitive now wraps
  the call in `catch_unwind` and degrades to `Value::Null` on
  unwind, so a tokio task can't take the daemon down.
- **`to_int(Float)` rejects non-finite values.** `NaN` and `±∞`
  used to slip through `as i64` (NaN → 0, ∞ → `i64::MIN`/`i64::MAX`),
  both of which violate Principle 1. Finite-but-out-of-range floats
  still saturate (matching the documented `as`-cast semantics);
  non-finite values fall through to the same partial-data `Null`
  path as unparseable strings.

### Refactored — TLS helper centralization

`crate::tls` now owns the `tls { cert key ca }` block parser
(`TlsConfig::from_properties_block`) and the rustls `CryptoProvider`
installer (`install_default_crypto_provider`), both of which were
duplicated across `syslog_tls`, `otlp_grpc` (input), and `otlp`
(output) after the OTLP push. Consolidation keeps error wording
uniform across modules and removes the only direct duplication
flagged by the v0.5.0 abstraction review.

### Known limitations

- **`otlp_http` server-side TLS** is not implemented; front the input
  with a TLS-terminating proxy (envoy / nginx / traefik) or use
  `otlp_grpc` for native TLS. Native HTTPS support is queued for
  v0.5.x.
- **Selective re-send of OTLP `partial_success.rejected_log_records`**
  is logged as a warning only; the dedicated retry-just-the-rejects
  path is queued for v0.5.x. Transport-level retry shipped in this
  release covers hard failures (connection refused, 5xx, …).

## [0.4.0] - 2026-04-24

Testability release. Builds the static analyzer and observability
tooling on top of the DSL finalised in v0.3.0. No DSL breaking changes
— `limpid --check` does more, pipelines behave the same.

### Added — `limpid --check` static analyzer

- Full type-aware analyzer lives in `crates/limpid/src/check/` and
  runs whenever `limpid --check <config>` is invoked. It replaces the
  former "syntax OK" pass with real dataflow and type checking.
- Static type inference: `FieldType` + `Bindings` thread structural
  types through pipelines; function argument / return type signatures
  (`FunctionSig`), assignment type conflicts, operator type checks, and
  parser-function return shapes are all verified.
- Parser functions (`parse_json`, `parse_kv`, `syslog.parse`,
  `cef.parse`, `regex_parse`) declare the workspace keys they produce
  via `ParserInfo`; downstream references to those keys are verified.
- Diagnostic rendering: rustc-style source snippet + caret,
  "did you mean" Levenshtein suggestions for unknown identifiers /
  functions, and clear summary + footer lines.
- Expr-level span: diagnostics carry precise source spans from
  expression nodes (not just statements), so the caret points at the
  offending sub-expression (`lower(workspace.count)` → carets the arg).
- `include "<glob>";` in configs is expanded by the analyzer with a
  cycle-safe source map, and summary counts (input / output / process /
  pipeline) are emitted per check.
- Footer: clean configs end with
  `<path>: Configuration OK (N pipeline(s), M process(es); dataflow check passed)`;
  configs with warnings include the warning count; configs with errors
  exit 1 with `error: N error(s) found`.

### Added — CLI flags

- `--strict-warnings`: promotes warning count to exit-2 (diagnostic
  level stays warning). CI-friendly switch for "warnings are failures."
- `--ultra-strict`: promotes **unknown-identifier** warnings to errors
  (exit 1). Distinct axis from `--strict-warnings` — this one changes
  the diagnostic level, not just the exit code. The two flags compose:
  unknown idents become errors, other warnings can still trigger
  exit-2. Category is tagged via `DiagKind`; `UnknownIdent` is the
  currently promoted class.
- `--graph[=<format>]`: emits a structural view of every pipeline to
  stdout. Formats: `mermaid` (default, GitHub-renderable),
  `dot` (Graphviz), `ascii` (terminal-only tree). Analyzer output stays
  on stderr so `--graph | pbcopy` etc. works cleanly.

### Added — documentation

- `docs/src/operations/schema-validation.md` — operations guide for
  schema validation. Covers the design decision to not ship an in-tree
  validator, the `limpidctl tap --json | <validator>` recipe (OCSF /
  ECS / custom JSON Schema), and the alternatives that were rejected
  (in-tree validator, DSL schema annotations, runtime per-event
  checking). Cross-linked from `operations/tap.md`.

### Changed — internals

- `Module::schema()` removed. Input / output modules no longer declare
  a data contract: they are I/O-pure (bytes in / bytes out) and have
  nothing to advertise. Schema information is carried by
  `FunctionSig` / `ParserInfo` on the function registry, which is where
  the analyzer looks. `modules/schema.rs` now only exports the
  `FieldType` / `FieldSpec` vocabulary.
- AST `Expr` became a wrapper struct (`Expr { kind: ExprKind, span }`)
  to carry per-expression spans without rewriting every pattern match.
- Unused `name_span` / `key_span` fields on def / property AST nodes
  (left as `#[allow(dead_code)]` placeholders) were removed; they can
  come back if a future analyzer phase needs them.
- Diagnostic category is routed via `DiagKind` enum (`UnknownIdent` /
  `TypeMismatch` / `Dataflow` / `Other`) instead of message-string
  heuristics, so category rendering and `--ultra-strict` promotion
  share the same source of truth.

### Security / hardening

- Snippet renderer sanitises ASCII control bytes (0x00–0x1F minus `\t`,
  and 0x7F) to `?` before writing the source line to stderr. Prevents
  ANSI OSC/CSI injection through config contents displayed in a
  reviewer's terminal.
- `include "<glob>";` is now confined to the config's root directory.
  Absolute paths and `..` traversal outside that root are rejected with
  a clear error. Prevents an include line from silently pulling in
  arbitrary files (`/etc/passwd`, `~/.ssh/*` etc.) or from leaking the
  first bytes of such files via a pest parse error.

### Documentation fixes

- `limpidctl check` references in operations / pipelines / processing
  docs corrected to `limpid --check` (check lives in the daemon binary,
  not the CLI tool — this was decided during the v0.3.0 restructure,
  but the docs had drifted).

## [0.3.0] - 2026-04-24

DSL stabilization release. This is a broad pre-1.0 breaking change that
settles the Event model, function namespaces, and core shape so that
future work (analyzer polish, snippet library, transport expansion) can
build on a final-form DSL without further surface-level churn.

### Breaking — Event model renamed

- `Event.raw` → `Event.ingress` (immutable bytes received on this hop)
- `Event.message` → `Event.egress` (bytes written on the wire by the output)
- `Event.fields` → `Event.workspace` (pipeline-local scratch namespace)
- `tap --json` / `inject --json` key names follow the rename; existing
  dumped replay files need `sed` (see `docs/src/operations/upgrade-0.3.md`)

### Breaking — Event core is now schema-agnostic

- `Event.facility` / `Event.severity` removed. These were syslog-specific
  metadata masquerading as pipeline-wide state; in a world where OTLP /
  OCSF / vendor JSON are first-class citizens, they do not belong in the
  Event core.
- DSL assignments `facility = N` / `severity = N` are now "unknown
  assignment target" errors. The PRI byte is constructed explicitly via
  the new `syslog.set_pri(egress, facility, severity)` function.
- `syslog.extract_pri(bytes)` returns the numeric PRI for reading.

### Breaking — Native process layer removed

- `modules/process/` is gone in its entirety. Pipeline statements like
  `process parse_syslog` no longer resolve to built-ins — schema-specific
  parsers are DSL functions (`syslog.parse(ingress)` etc.) invoked as
  statements inside an inline `process { ... }` block, and format
  primitives (`parse_json`, `parse_kv`, `regex_replace`) are flat DSL
  functions.
- `prepend_source` / `prepend_timestamp` have no direct replacement; the
  upgrade guide shows the `+` / `strftime` rewrite.

### Added — dot-namespaced function call syntax

- `<namespace>.<fn>(args)` grammar. Schema-specific functions declare their
  identity in the name. `parse_syslog(raw)` / `parse_cef(raw)` /
  `strip_pri(msg)` become `syslog.parse(ingress)` / `cef.parse(ingress)` /
  `syslog.strip_pri(egress)`. Flat primitives (JSON/KV/regex/hash/table)
  keep the bare-name form.
- New functions: `syslog.set_pri`, `syslog.extract_pri`, `regex_parse`,
  `hostname()`.

### Added — `regex_parse(target, pattern)`

- Named-capture extraction with dotted capture names producing nested
  objects: `(?P<date.month>\\w{3})` merges into `workspace.date.month`.
  Returns `Object` (bare-statement merges into `workspace`) or `null`.
- `regex_extract` remains as the single-value extractor.

### Added — `let` bindings

- `let x = <expr>` inside a `def process { ... }` body. Process-local
  scratch that keeps `workspace` clean of intermediate values. Bare-ident
  resolution is `LocalScope → Event metadata → error`.

### Added — pipeline fan-in

- `input a, b, c;` accepts multiple comma-separated inputs feeding the
  same pipeline body. Motivation: HA syslog (two redundant feeds running
  the same dedup / transform pipeline) no longer requires copy-pasting
  the pipeline twice.

### Added — `${expr}` template interpolation + string `+`

- `"prefix-${workspace.foo}-suffix"` interpolates any DSL expression.
  Old `%{name}` shorthand in `format()` has been removed; placeholders
  must be either reserved event names (`ingress`, `egress`, `source`,
  `timestamp`, `severity`, `facility`) or explicit `workspace.xxx` /
  `let`-bound names.
- `+` operator concatenates strings (falls back to arithmetic for
  numeric operands).

### Added — `strftime`, `hostname`

- `strftime(timestamp, format, tz?)` formats an RFC 3339 timestamp.
- `hostname()` returns the daemon's system hostname; portable configs
  can use `"${hostname()}"` in templates instead of hardcoding.

### Added — `output file` path templates via DSL evaluator

- `output file { path "/var/log/${source}/${strftime(timestamp, \"%Y-%m-%d\")}.log" }`
  evaluates the DSL expression per event instead of going through the
  legacy string template.

### Added — Design Principles page

- `docs/src/design-principles.md` publishes the five principles that
  govern limpid's scope (zero hidden behavior, I/O purity, domain
  knowledge as DSL snippets, only `egress` crosses hops, schema
  identity via namespaces).

### Added — developer / example docs

- `docs/src/processing/design-guide.md` — process design guide for
  contributors writing snippet library entries.
- `docs/src/pipelines/multi-host.md` — end-to-end worked example of a
  edge-host → relay → AMA multi-host pipeline, highlighting how
  the `tap` / `inject` primitives and the RFC 5424 hop contract turn a
  distributed pipeline into something you can reason about from one
  config.

### Changed — function code organization

- `crates/limpid/src/functions/` is now a tree of one-file-per-function
  modules: `primitives/` (flat), `syslog/` (dot namespace), `cef/`
  (dot namespace). The old `mod.rs` megafile is gone.
- Module trait introduced (`crates/limpid/src/modules/mod.rs`):
  `Module: Sized { fn schema() -> ModuleSchema; fn from_properties(...) }`.
  Replaces the former `FromProperties`. `schema()` is unused in-tree
  today but reserved for the upcoming analyzer (v0.4.0).

### Changed — hardening

- `limpid` and `limpidctl` restore `SIG_DFL` for SIGPIPE, so piped
  output (`limpidctl stats | head`) exits cleanly instead of panicking.
- `output http`: emits a `WARN` log when `verify false` disables TLS
  certificate validation, and the setting is documented as
  debugging-only.
- Control socket (`/var/run/limpid/control.sock`): max 8 concurrent
  connections, max 16 MiB per inject stream, max 4 KiB per command line.
- `syslog_tls` certificate and key loading moved off the async runtime
  via `spawn_blocking` to avoid stalling the reactor at startup.
- `fmt: cargo fmt --all` applied once across the tree so subsequent
  diffs are free of cosmetic noise.

### Internal refactors

- `<PRI>` header parsing consolidated into a single `parse_leading_pri`
  helper (was duplicated across `strip_pri`, `extract_pri`, `set_pri`).
- `values_equal` merged into `values_match` as the single equality
  routine for both `==`/`!=` and `switch` arms.
- TCP and Unix-socket outputs share a `PersistentConn` trait encoding
  the common "connect on first write, reconnect on broken pipe" pattern.
- `tls::build_client_config` (speculative dead code) removed; TLS client
  support will be reintroduced when an output needs it.

### Removed

- `modules/process/` (entire directory) and the `ModuleRegistry`
  process API (`register_process` / `call_process` / `process_names` /
  `ProcessFn`).
- `%{name}` shorthand in `format()` templates.
- `FromProperties` trait (absorbed into `Module`).

### Migration

See `docs/src/operations/upgrade-0.3.md` for end-to-end migration
recipes including `sed` snippets for the Event model rename, the
function rename table, and worked examples of replacing every removed
native process with its DSL function equivalent.

## [0.2.2] - 2026-04-24

### Added

- `limpidctl inject --replay-timing[=<factor>]` — replays events at their
  original timing using each event's top-level `timestamp` field. Accepts
  `realtime` (= `1x`) or a factor like `10x` / `0.2x`. Defaults to `1x`
  when given without a value. Requires `--json`.

### Documentation

- `docs/src/operations/tap.md` — cadence-faithful replay section with
  examples (default / 10x / 0.2x / realtime), `--json` requirement, and
  the explicit failure cases (missing or unparseable timestamp, invalid
  factor, backwards timestamp, wall-clock catch-up) so there is no
  hidden behaviour.
- `docs/src/operations/cli.md` — `--replay-timing` entry in the CLI
  quick reference.

## [0.2.1] - 2026-04-18

### Fixed

- `--test-pipeline` now loads `table { ... }` global blocks from the
  configuration. Previously it constructed an empty `TableStore`, which
  caused pipelines using `table_lookup` / `table_upsert` / `table_delete`
  to emit "unknown table" warnings in test mode only.

## [0.2.0] - 2026-04-17

### Added

- `limpidctl inject <input|output> <name>` — pushes raw lines into a
  named input's event channel, or directly into an output's queue
  (bypassing pipelines entirely). Symmetric with `limpidctl tap`.
- `inject --json` — pushes full Event JSON (as emitted by `tap --json`),
  enabling `tap → inject` roundtrip for replay use cases.
- Control protocol: `inject <kind> <name> [json]`, EOF-terminated.
- Per-inject metrics: `events_injected` (for inputs and outputs) and
  `events_received` (for outputs).
- Prometheus exporter: three new counters (input injected, output
  injected, output received).

### Changed

- `limpidctl stats` output restructured to **Pipelines → Inputs →
  Outputs** ordering with updated counter set.

### Fixed

- `.gitignore` patterns to exclude common secrets layouts.
- `fold_by_precedence`: guard against empty operator lists.
- `tap.rs`: best-effort comment / error-path fixes surfaced by the
  v0.2.0 audit pass.

## [0.1.0] - 2026-04-17

Initial public release. Rust + tokio log pipeline daemon replacing
rsyslog / syslog-ng / fluentd with a single readable DSL (`def input`,
`def process`, `def output`, `def pipeline`). Includes syslog (UDP/TCP/
TLS) / tail / journal / unix socket inputs; file / HTTP / Kafka / TCP /
UDP / unix socket / stdout outputs; in-DSL expression language with
parsers (JSON / KV / CEF / syslog), regex, string templates, tables
with TTL, GeoIP; control socket (`limpidctl tap`, `stats`, `health`);
hot reload via `SIGHUP` with automatic rollback; per-output disk-backed
queues.

[Unreleased]: https://github.com/naoto256/limpid/compare/v0.7.7...HEAD
[0.7.7]: https://github.com/naoto256/limpid/compare/v0.7.6...v0.7.7
[0.7.6]: https://github.com/naoto256/limpid/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/naoto256/limpid/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/naoto256/limpid/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/naoto256/limpid/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/naoto256/limpid/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/naoto256/limpid/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/naoto256/limpid/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/naoto256/limpid/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/naoto256/limpid/compare/v0.5.8...v0.6.0
[0.5.8]: https://github.com/naoto256/limpid/compare/v0.5.7...v0.5.8
[0.5.7]: https://github.com/naoto256/limpid/compare/v0.5.6...v0.5.7
[0.5.6]: https://github.com/naoto256/limpid/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/naoto256/limpid/compare/v0.5.4...v0.5.5
[0.5.4]: https://github.com/naoto256/limpid/compare/v0.5.3...v0.5.4
[0.5.3]: https://github.com/naoto256/limpid/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/naoto256/limpid/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/naoto256/limpid/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/naoto256/limpid/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/naoto256/limpid/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/naoto256/limpid/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/naoto256/limpid/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/naoto256/limpid/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/naoto256/limpid/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naoto256/limpid/releases/tag/v0.1.0
