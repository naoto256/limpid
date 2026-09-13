# Keep private fields out of forwarded logs

Keep the original locally, but send only the fields a downstream system needs. Removing one `token` key is not enough when the same value also appears in a message, URL, or nested object. This recipe builds a new JSON document from a small allowlist instead of copying the input object.

## Define the outbound contract

The example accepts application audit events with an ID shaped like `evt-0001`, an action of `login` or `logout`, and an outcome of `success` or `failure`. Only those three fields leave the process. Usernames, email addresses, tokens, free-text messages, and unknown fields are excluded regardless of their location.

This is a contract for one event format, not a universal secret detector or anonymization guarantee. Review the meaning of every retained field before adapting it. A valid-looking identifier can still be sensitive in your application.

## Archive first, then construct the public copy

Create a private directory for the archive, control socket, and error log. This runnable example uses a loopback HTTP receiver; it sends nothing to a cloud service.

```limpid
control {
    socket "/tmp/limpid-safe/control.sock"
    error_log "/tmp/limpid-safe/error.jsonl"
}

def input application {
    type syslog_tcp
    bind "127.0.0.1:15514"
}

def output archive {
    type file
    path "/tmp/limpid-safe/original.jsonl"
}

def output downstream {
    type http
    peer { url "http://127.0.0.1:18080/" }
    content_type "application/json"
    batch_size 1
    retry { max_attempts 1 }
}

def process public_document {
    workspace.audit = parse_json(ingress)
    if not regex_match(workspace.audit.event_id, "^evt-[0-9]{4}$") {
        error "unsupported event ID"
    }
    if workspace.audit.action != "login" and workspace.audit.action != "logout" {
        error "unsupported action"
    }
    if workspace.audit.outcome != "success" and workspace.audit.outcome != "failure" {
        error "unsupported outcome"
    }
    egress = to_json({
        event_id: workspace.audit.event_id,
        action: workspace.audit.action,
        outcome: workspace.audit.outcome
    })
}

def pipeline safe_forwarding {
    input application
    output archive
    process public_document
    output downstream
}
```

The first output takes an event copy before processing. Replacing `egress` later does not rewrite that earlier archive copy. The HTTP output sends the new `egress`, not the workspace containing the parsed original.

Malformed JSON and unsupported values stop processing before the downstream output and enter the configured [error log](../operations/error-log.md). The validation messages are fixed strings; they do not interpolate input values. The error record itself can contain the original event, so it must remain private.

Archive and downstream queues are not an atomic transaction. Taking the archive copy does not establish that its disk write has completed before the HTTP request. This recipe does not guarantee an archive survives every disk failure.

## Inject synthetic data

Save the configuration as `safe.conf`. Use a fresh, access-restricted directory in place of `/tmp/limpid-safe`, reserve the input port, and start a loopback receiver on port 18080 before starting the test instance. The receiver should record each request body and return HTTP 200.

Save this single line as `events.jsonl`. All private-looking values below are deliberately fake:

<!-- prettier-ignore -->
```json
{"event_id":"evt-0001","action":"login","outcome":"success","email":"alice@example.invalid","token":"FAKE_TOKEN_ONLY","message":"email=alice@example.invalid token=FAKE_TOKEN_ONLY","extra":{"copy":"FAKE_TOKEN_ONLY"}}
```

```sh
limpid --check --config safe.conf
limpid --config safe.conf
```

From another terminal, inject the line into that test instance:

```sh
limpidctl --socket /tmp/limpid-safe/control.sock inject input application < events.jsonl
```

Do **not** add `--json` here: these lines are application JSON carried as raw ingress, not full limpid Event envelopes. Compare the receiver body with:

```json
{ "event_id": "evt-0001", "action": "login", "outcome": "success" }
```

Check the exact key set, not only whether one token string is absent. The archive must still contain the original line including all fake private values. Confirm this from the files after normal shutdown, not merely from an enqueue count.

## Exercise the failure paths

Repeat with an allowed logout/failure event, a new unknown nested field containing another fake token, malformed JSON, a missing ID, a numeric ID, an unexpected action, an object instead of an outcome, and an ID containing a token. Only valid events should reach HTTP. Invalid records should remain available locally in the archive and error log.

Do not catch every error and fall back to `egress = ingress`: that sends the very input the validation rejected. Likewise, forwarding `to_json(workspace.audit)` or adding the original message to the public document defeats the allowlist.

## Before using a remote destination

Replace the loopback URL with the destination's HTTPS endpoint and its required authentication. Keep TLS verification enabled. The one-attempt retry setting is for a bounded fixture test, not a general production recommendation. Choose queue sizing, retry, and recovery behavior separately.

Protect the raw archive, error log, backups, tap access, and diagnostics. Replaying a local error record directly into an output can bypass this process; replay through the input when validation must run again. Local retention of private data is intentional here. If retaining the original is prohibited, this archive-first design is not appropriate.

## Verified with a running daemon

This configuration was exercised with a real limpid 0.9.0 process on macOS and a loopback HTTP receiver, not just pipeline test mode. The eight synthetic input lines produced the following results:

| Check                                           | Observed result                                                                |
| ----------------------------------------------- | ------------------------------------------------------------------------------ |
| Allowed login and logout events                 | Two HTTP requests, each containing exactly `event_id`, `action`, and `outcome` |
| Fake email, token, free text, and nested copies | Absent from both HTTP bodies                                                   |
| Six malformed or unsupported events             | No HTTP delivery; six error-log records                                        |
| Original archive                                | All eight original lines matched, independent of output order                  |
| Shutdown                                        | Normal exit code 0                                                             |

As a negative control, replacing the new document with `to_json(workspace.audit)` caused the receiver assertion to fail on the leaked extra fields. That deliberately unsafe variant was sent only to the loopback test receiver. No real secrets or production events were used. This check does not establish Windows behavior, remote destination acceptance, or production retention and recovery guarantees.
