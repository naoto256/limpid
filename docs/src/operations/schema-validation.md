# Schema Validation

limpid does not ship a schema validator. There is no `limpidctl validate`, no built-in OCSF / ECS / ASIM / CIM check, and no schema-annotation syntax in the DSL. Validation is performed by piping `tap --json` output into whichever validator matches the schema your downstream expects:

```bash
limpidctl tap output <name> --json | <validator>
```

This page explains why limpid takes that shape, how to assemble validation for common targets, and how the pattern plugs into CI and production monitoring.

## The problem

A DSL field rename passes `limpid --check`, passes unit tests, and still breaks production: Splunk drops the field, Sentinel ingests it into the wrong column, or an ECS-strict pipeline rejects the event entirely. The failure is silent at the limpid layer because limpid has no opinion about what the downstream considers "valid".

This is a real gap. It is worth closing in CI and — for high-stakes pipelines — in production. The question is where the validator lives.

## Why limpid does not bundle a validator

Downstream log schemas are diverse, and they are diverse in ways that make a bundled approach structurally untenable:

| Schema | Owner | Portable form | Usable by a bundled validator? |
|---|---|---|---|
| **OCSF** | Splunk / AWS, OSS | JSON Schema | Yes |
| **ECS** | Elastic | YAML (derivable JSON Schema) | Partially |
| **ASIM** | Microsoft Sentinel | KQL function schema | No portable form |
| **CIM** | Splunk | Data-model definitions | No portable form |
| Internal | — | Anything | Case by case |

Two observations follow:

1. JSON Schema is the lingua franca for JSON instance validation, but **not every log schema reduces to a JSON Schema**. ASIM and CIM do not have portable schema files at all — their "schema" is expressed through ingestion-side query logic. A bundled validator cannot represent them without re-implementing the ingestion logic itself.
2. Even between JSON-Schema-describable targets, "compatible" is not standardized. Schema-diff tools exist, but their definitions of subset / superset / equivalence disagree. A universal schema-comparison engine inside limpid is not a realistic goal.

The conclusion is that any bundled validator would at best cover a subset of targets, and that subset would require continuous maintenance against every spec revision (OCSF event classes, ECS field additions, internal schema drift). That is a commitment limpid will not take — it pulls domain knowledge into the daemon, which conflicts directly with [Principle 3](../design-principles.md) (domain knowledge ships as DSL snippets, not Rust).

## Why `tap --json | validator` wins

With `limpidctl tap output <name> --json` emitting one JSON-encoded [Event](../processing/README.md) per line, UNIX pipes compose the validator the user actually needs:

### 1. limpid commits to no schema ecosystem

When OCSF v2 lands, when ASIM changes its tables, when a new SIEM appears with its own schema — limpid's source tree is unaffected. The validator is the user's concern; the format of `tap --json` is the only contract limpid maintains.

### 2. Official validators beat re-implementations

Most downstream schemas ship an official or vendor-blessed validator (OCSF's reference validator, Microsoft's ASIM sample-ingestion flow, Elastic's ECS tooling, `ajv-cli` for any JSON Schema, internal linters for internal schemas). Piping to the real validator yields results that match what the downstream itself will accept. A re-implementation inside limpid could only approximate it.

### 3. CI **and** production monitoring, same mechanism

A bundled validator would inevitably be wired into `check` or a build-time phase, useful only for pre-deployment fixtures. Pipe composition is agnostic: the same command that validates a fixture in CI can validate a live stream in production, with no extra machinery.

```bash
# CI — validate fixture output against OCSF Network Activity
limpidctl inject input edge_syslog --json < tests/fixtures/cisco.jsonl &
limpidctl tap output siem --json | ajv validate -s ocsf/network_activity.json -

# Production — continuous schema-drift monitoring
limpidctl tap output siem --json \
  | ajv validate -s ocsf/network_activity.json - \
  | alertmanager-client
```

Schema drift surfaces immediately, rather than at the next CI run against a stale fixture.

### 4. Validators the user already trusts

`jq` for shape checks. `ajv-cli` or `jsonschema` for JSON Schema. Vendor-supplied sample-ingestion tools. Internal linters bound to internal conventions. All of these remain stdin-consuming UNIX tools and plug in with no glue code.

### 5. Unknown targets work on day one

When a new downstream appears with a schema nobody has seen before, the integration is writing (or obtaining) a validator that reads JSONL from stdin. limpid does not need to know it exists.

## Recipes

All recipes assume `limpidctl tap output <name> --json` is producing the pipeline's final serialized output — the same Event JSON described in [Debug Tap](./tap.md). Each event is one line, with `received_at`, `source`, `ingress`, `egress`, and `workspace` top-level keys; structured output fields live under `workspace`.

### OCSF (JSON Schema)

Use any JSON Schema validator. `ajv-cli` is convenient because it streams:

```bash
limpidctl tap output ocsf_sink --json \
  | jq -c '.workspace.ocsf' \
  | ajv validate -s ocsf-schemas/network_activity.json --all-errors -
```

`jq -c '.workspace.ocsf'` extracts the structured payload the pipeline built under a workspace key; adjust the path to match your DSL.

### ECS (Elastic)

ECS ships YAML; convert to JSON Schema once with Elastic's generator and feed it to the same validator:

```bash
limpidctl tap output elastic --json \
  | jq -c '.workspace.ecs' \
  | jsonschema -i /dev/stdin ecs.schema.json
```

### ASIM (Microsoft Sentinel)

ASIM has no portable schema file. The check that matches production is the ingestion path itself — send a sample batch through the Log Analytics ingestion API and let Sentinel report rejections:

```bash
limpidctl tap output sentinel --json \
  | head -n 100 \
  | az-monitor-ingest --stream Custom-ASimNetworkSessionLogs
```

This is exactly what a bundled validator could not do — the "validator" is the real ingestion endpoint.

### Splunk CIM

Drive a Splunk `| search` that asserts CIM field coverage, or use a CIM-aware linter:

```bash
limpidctl tap output splunk --json \
  | jq -r '.egress' \
  | splunk-cim-check --model Network_Traffic
```

### Internal schemas

Any JSONL-stdin tool works. Internal linters are the common case:

```bash
limpidctl tap output warehouse --json | ./tools/warehouse-lint
```

## CI integration

The building block is `inject` on one side, `tap` on the other. `--replay-timing` is unnecessary in CI — raw throughput is fine.

```bash
# ci/validate-ocsf.sh
set -euo pipefail

sudo limpidctl tap output ocsf_sink --json > tap.jsonl &
TAP_PID=$!

sudo limpidctl inject input edge_syslog --json < tests/fixtures/edge.jsonl
sleep 1   # drain
kill $TAP_PID

jq -c '.workspace.ocsf' tap.jsonl \
  | ajv validate -s schemas/ocsf/network_activity.json --all-errors -
```

The `ajv validate` exit code is the job's exit code. No limpid-side machinery is required beyond what already ships.

For tighter fixture-based gating, a fixture directory per vendor (`tests/fixtures/fortigate.jsonl`, `tests/fixtures/cisco_asa.jsonl`, …) with one CI job per combination keeps failures specific.

## Production monitoring

Run the validator alongside the daemon as a separate service. The systemd pattern mirrors any other long-running stdin tool:

```ini
# /etc/systemd/system/limpid-ocsf-monitor.service
[Service]
ExecStart=/bin/sh -c 'limpidctl tap output ocsf_sink --json \
  | ajv validate -s /etc/limpid/schemas/ocsf_network_activity.json - \
  | logger -t limpid-ocsf-monitor'
Restart=always
```

A few operational notes:

- **Back-pressure.** `tap` is lossless toward the subscriber: if the validator lags, pipe-buffer back-pressure will propagate into the tap reader task. For high-volume outputs, sample before validating: `jq -c 'select((now * 1000 | floor) % 100 == 0)'` or similar. Validation is a statistical check, not an audit — sampling is fine.
- **Failure routing.** A validator's exit code only reports the first bad document unless you use a batch-tolerant mode (`ajv validate --all-errors -`). For monitoring, emit a counter per rejection rather than terminating the stream.
- **Zero cost when nothing subscribes.** Tap points cost one atomic load per event when no subscriber is attached (see [Debug Tap](./tap.md)). Turning validation off is as cheap as stopping the service.

## Writing your own validator

Anything that reads JSONL from stdin and exits non-zero on failure works. The contract is:

- **One event per line.** Each line is a complete [Event JSON](./tap.md#usage) object.
- **Exit code 0 = all good, non-zero = reject.** For streaming validators, prefer printing per-event diagnostics to stderr and keeping the process alive; let a supervising process aggregate.
- **No assumptions about key order.** `tap --json` does not guarantee key order across versions; schema validators don't care, but ad-hoc `grep` pipelines might.
- **Workspace keys.** Structured fields built by the pipeline live under `workspace.<key>`. Use the key names your DSL actually assigns; `limpid --check --graph` shows which workspace keys each output observes.
- **Non-JSON wire formats.** If the final wire format is protobuf, Avro, or similar, serialize with your existing producer and validate the bytes (e.g. `.egress` piped through `protoc --decode`).

## Related design decisions

This approach is consistent with two existing limpid patterns:

- **Record and replay.** limpid has no `limpidctl record` / `limpidctl replay`. Recording is `tap --json > file.jsonl`; replay is `inject --json --replay-timing < file.jsonl`. Two primitives, one pipe, no third subcommand. See [Debug Tap](./tap.md#replay-with-tap--inject).
- **Principle 1 — Zero hidden behavior.** A bundled validator would run "in the background" on behalf of the user. The pipe form makes every check an explicit command with an observable exit status.
- **Principle 3 — Domain knowledge ships as DSL snippets.** OCSF, ECS, ASIM, CIM — these are domain specifications. Their maintenance lives outside the daemon by design.

The rule that produced this design is worth stating on its own: **if an existing primitive plus a UNIX pipe already solves the problem, limpid does not grow a new subcommand**. It is the same rule that rejected `limpidctl record` / `limpidctl replay` in favour of `tap` + `inject`.
