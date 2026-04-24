# Built-in Processes

Called via `process <name>` in a pipeline or within a process definition.

## parse_cef

Parses CEF (Common Event Format) from `ingress` into `workspace`.

```
process parse_cef
```

Input: `<134>CEF:0|Fortinet|FortiGate|7.0|1234|FW|5|src=10.0.0.1 dst=192.168.1.1`

Result:
- `workspace.device_vendor` = `"Fortinet"`
- `workspace.device_product` = `"FortiGate"`
- `workspace.src` = `"10.0.0.1"`
- `workspace.dst` = `"192.168.1.1"`

Finds `CEF:` anywhere in `ingress` (handles syslog header prefix). Extensions are parsed as key-value pairs.

## parse_json

Parses JSON from `egress` into `workspace`.

```
process parse_json
```

Input: `{"host":"fw01","level":"error","msg":"connection refused"}`

Result:
- `workspace.host` = `"fw01"`
- `workspace.level` = `"error"`
- `workspace.msg` = `"connection refused"`

Supports nested JSON objects (stored as nested workspace values).

## parse_syslog

Parses RFC 3164 (BSD) and RFC 5424 syslog headers into `workspace`.

```
process parse_syslog
```

Auto-detects the format based on the version digit after PRI.

**RFC 3164** input: `<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password`

Result:
- `workspace.hostname` = `"myhost"`
- `workspace.appname` = `"sshd"`
- `workspace.procid` = `"1234"`
- `workspace.syslog_msg` = `"Failed password"`

**RFC 5424** input: `<134>1 2026-04-15T10:30:00Z host app 999 ID1 - Hello`

Result:
- `workspace.hostname` = `"host"`
- `workspace.appname` = `"app"`
- `workspace.procid` = `"999"`
- `workspace.msgid` = `"ID1"`
- `workspace.syslog_msg` = `"Hello"`

Also sets `egress` to the parsed syslog MSG body.

## parse_kv

Parses `key=value` pairs from `egress` into `workspace`.

```
process parse_kv
```

Input: `date=2026-04-15 srcip=10.0.0.1 action=deny msg="login failed"`

Result:
- `workspace.date` = `"2026-04-15"`
- `workspace.srcip` = `"10.0.0.1"`
- `workspace.action` = `"deny"`
- `workspace.msg` = `"login failed"`

Handles quoted values. Tokens without `=` are skipped. Useful for FortiGate, Palo Alto, and similar firewall log formats.

## strip_pri

Removes the `<PRI>` header from `egress`.

```
process strip_pri
```

`<134>hello` → `hello`

## prepend_source

Prepends the source IP address to `egress`.

```
process prepend_source
```

`hello` → `192.0.2.10 hello`

## prepend_timestamp

Prepends a BSD-style timestamp (local time) to `egress`.

```
process prepend_timestamp
```

`hello` → `Apr 15 10:30:00 hello`

## regex_replace

Replaces all regex matches in `egress`.

```
process regex_replace("\\d{4}-\\d{2}-\\d{2}", "REDACTED")
```

`date=2026-04-15 msg=test` → `date=REDACTED msg=test`

Two required arguments: pattern and replacement. Supports capture group references (`$1`, `$2`).

Regex patterns are cached per thread for performance.
