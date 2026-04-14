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

- Each event is written as one line (message content + newline).
- Not recommended for production use — use [file](./file.md) or [tcp](./tcp.md) instead.
- Useful with `--test-pipeline` for seeing processed output.
