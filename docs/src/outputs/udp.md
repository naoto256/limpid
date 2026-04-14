# udp

Sends events as UDP datagrams.

## Configuration

```
def output remote {
    type udp
    address "10.0.0.1:514"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `address` | yes | — | Target address (`host:port`) |

## Notes

- The UDP socket is bound to an ephemeral port on first use and reused.
- UDP provides no delivery guarantee. Use [tcp](./tcp.md) or [http](./http.md) with a disk queue for reliable delivery.
