# Separate malformed logs without hiding failures

A mixed stream can contain both valid application JSON and broken lines. Put malformed input in a separate local file, forward the events you understand, and keep unexpected schema changes visible in the error log.

This example deliberately distinguishes two failures: invalid JSON is an anticipated input format problem; valid JSON with an unsupported action violates the application contract. Do not catch both and quietly forward the original.

## Route each outcome

Use a fresh private directory in place of `/tmp/limpid-quarantine`. The outputs are local files so the example can be tested without a cloud account.

```limpid
control {
    socket "/tmp/limpid-quarantine/control.sock"
    error_log "/tmp/limpid-quarantine/error.jsonl"
}

def input application {
    type syslog_tcp
    bind "127.0.0.1:15514"
}

def output original {
    type file
    path "/tmp/limpid-quarantine/original.jsonl"
}

def output malformed {
    type file
    path "/tmp/limpid-quarantine/malformed.jsonl"
}

def output accepted {
    type file
    path "/tmp/limpid-quarantine/accepted.jsonl"
}

def process classify_json {
    workspace.invalid_json = false
    try {
        workspace.document = parse_json(ingress)
    } catch {
        workspace.invalid_json = true
    }
}

def process malformed_document {
    egress = to_json({ reason: "invalid JSON", message: ingress })
}

def process accepted_document {
    if workspace.document.action != "login" and workspace.document.action != "logout" {
        error "unsupported application action"
    }
    egress = to_json({ action: workspace.document.action })
}

def pipeline classify_and_route {
    input application
    output original
    process classify_json
    if workspace.invalid_json {
        process malformed_document
        output malformed
    } else {
        process accepted_document
        output accepted
    }
}
```

The catch covers only JSON parsing. It has a concrete routing consequence: the malformed branch preserves the line in a JSON envelope with a fixed reason. Errors in the later validation process are not swallowed; they take limpid's normal [error-log path](../operations/error-log.md).

The original output takes its copy before either branch changes `egress`. These queues are independent, not a transaction. An archive enqueue is not proof that a write has reached disk. Protect all three local records and their backups: both malformed input and error records can contain sensitive data.

## Inject four cases

Save these raw application lines as `events.jsonl`:

```text
{"action":"login"}
{broken-json
{"action":"logout"}
{"new_action":"login"}
```

Save the configuration as `quarantine.conf`, create its private output directory, and reserve the listener port. Run the daemon, then inject from another terminal:

```sh
limpid --check --config quarantine.conf
limpid --config quarantine.conf
```

```sh
limpidctl --socket /tmp/limpid-quarantine/control.sock inject input application < events.jsonl
```

This is raw-line injection. `--json` would instead interpret each line as a full limpid Event, which these application documents are not.

After the writes complete, stop the test daemon normally and compare the files:

| Input                    | Original      | Accepted              | Malformed                               | Error log                                        |
| ------------------------ | ------------- | --------------------- | --------------------------------------- | ------------------------------------------------ |
| `{"action":"login"}`     | Original line | `{"action":"login"}`  | —                                       | —                                                |
| `{broken-json`           | Original line | —                     | JSON envelope containing `{broken-json` | —                                                |
| `{"action":"logout"}`    | Original line | `{"action":"logout"}` | —                                       | —                                                |
| `{"new_action":"login"}` | Original line | —                     | —                                       | Original event with the unsupported-action error |

Compare record sets rather than assuming independent output files share a write order. Check the malformed envelope's decoded `message`, including literal quotes and backslashes when you extend the fixture.

## Repair and replay deliberately

The malformed file is an application-defined JSON envelope, **not** a native limpid error-log record. Extract and repair its `message` before injecting it as raw input. Native error-log records use a different schema; follow the [documented replay procedure](../operations/error-log.md#replay) for those.

Replay through the input so parsing and validation run again. Direct output injection bypasses this pipeline. Keep a record of what you replayed: the earlier original archive copy already exists, and retries or replay can create duplicates. Repairing a format problem is not a reason to delete the evidence immediately.

## Validation scope

The four published fixture lines were injected into a real limpid 0.9.0 daemon on macOS. After normal exit code 0, the files contained all four exact original lines, two expected accepted documents, one exact malformed envelope, and one native error record for the unsupported action. This validates the local classification and file-output paths, not cloud delivery or Windows service behavior.
