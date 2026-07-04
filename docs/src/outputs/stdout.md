# stdout

Writes events to standard output. Useful for debugging and testing.

## Configuration

```
def output debug {
    type stdout
}
```

## Properties

None.

## Notes

- Each event is written as the `egress` bytes verbatim followed by a `\n`. Non-UTF-8 payloads are emitted unchanged (the writer does not lossily normalise to U+FFFD); the terminal's own rendering may still substitute for invalid sequences.
- Not recommended for production use — use [file](./file.md) or [syslog_tcp](./syslog-tcp.md) instead.
- Useful with `--test-pipeline` for seeing processed output.
- Common queue / retry properties — see [Queue and retry](./README.md#queue-and-retry).
