# Built-in Processes

Called via `process <name>` in a pipeline or within a process definition.

## parse_cef

Parses CEF (Common Event Format) from the raw message into `fields`.

```
process parse_cef
```

Input: `<134>CEF:0|Fortinet|FortiGate|7.0|1234|FW|5|src=10.0.0.1 dst=192.168.1.1`

Result:
- `fields.device_vendor` = `"Fortinet"`
- `fields.device_product` = `"FortiGate"`
- `fields.src` = `"10.0.0.1"`
- `fields.dst` = `"192.168.1.1"`

Finds `CEF:` anywhere in the raw message (handles syslog header prefix). Extensions are parsed as key-value pairs.

## parse_json

Parses JSON from the message into `fields`.

```
process parse_json
```

Input: `{"host":"fw01","level":"error","msg":"connection refused"}`

Result:
- `fields.host` = `"fw01"`
- `fields.level` = `"error"`
- `fields.msg` = `"connection refused"`

Supports nested JSON objects (stored as nested field values).

## parse_syslog

Parses RFC 3164 (BSD) and RFC 5424 syslog headers into `fields`.

```
process parse_syslog
```

Auto-detects the format based on the version digit after PRI.

**RFC 3164** input: `<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password`

Result:
- `fields.hostname` = `"myhost"`
- `fields.appname` = `"sshd"`
- `fields.procid` = `"1234"`
- `fields.syslog_msg` = `"Failed password"`

**RFC 5424** input: `<134>1 2026-04-15T10:30:00Z host app 999 ID1 - Hello`

Result:
- `fields.hostname` = `"host"`
- `fields.appname` = `"app"`
- `fields.procid` = `"999"`
- `fields.msgid` = `"ID1"`
- `fields.syslog_msg` = `"Hello"`

Also sets `message` to the parsed message body.

## parse_kv

Parses `key=value` pairs from the message into `fields`.

```
process parse_kv
```

Input: `date=2026-04-15 srcip=10.0.0.1 action=deny msg="login failed"`

Result:
- `fields.date` = `"2026-04-15"`
- `fields.srcip` = `"10.0.0.1"`
- `fields.action` = `"deny"`
- `fields.msg` = `"login failed"`

Handles quoted values. Tokens without `=` are skipped. Useful for FortiGate, Palo Alto, and similar firewall log formats.

## strip_pri

Removes the `<PRI>` header from the message.

```
process strip_pri
```

`<134>hello` → `hello`

## prepend_source

Prepends the source IP address to the message.

```
process prepend_source
```

`hello` → `192.0.2.10 hello`

## prepend_timestamp

Prepends a BSD-style timestamp (local time) to the message.

```
process prepend_timestamp
```

`hello` → `Apr 15 10:30:00 hello`

## regex_replace

Replaces all regex matches in the message.

```
process regex_replace("\\d{4}-\\d{2}-\\d{2}", "REDACTED")
```

`date=2026-04-15 msg=test` → `date=REDACTED msg=test`

Two required arguments: pattern and replacement. Supports capture group references (`$1`, `$2`).

Regex patterns are cached per thread for performance.
