# kafka

Produces events to an Apache Kafka topic. Uses librdkafka internally for batching, compression, retries, and connection management.

Requires the `kafka` feature at build time:

```bash
cargo build --release -p limpid --features kafka
```

## Configuration

```
def output events {
    type kafka
    brokers "kafka1:9092,kafka2:9092"
    topic "syslog-events"
    compression snappy
    acks all
    key source
    queue_timeout "5s"
}
```

## Properties

| Property | Required | Default | Description |
|----------|----------|---------|-------------|
| `brokers` | yes | — | Comma-separated list of Kafka brokers |
| `topic` | yes | — | Target topic name |
| `compression` | no | `none` | `none`, `gzip`, `snappy`, `lz4`, `zstd` |
| `acks` | no | `all` | `0` (fire-and-forget), `1` (leader only), `all` (all replicas) |
| `key` | no | none | Event field to use as partition key |
| `queue_timeout` | no | `5s` | Max wait when rdkafka's internal queue is full |

## Partition key

The `key` property determines which event field is used as the Kafka partition key. Events with the same key go to the same partition (ordering guarantee).

| Value | Key source |
|-------|------------|
| `source` | Source IP address |
| `facility` | Facility number |
| `severity` | Severity number |
| `fields.xxx` or any name | Named field value (must be a string) |

If the specified field is missing or null, the event is sent without a key (round-robin across partitions).

## Notes

- rdkafka handles batching and compression internally — no manual batch configuration needed (unlike [http](./http.md)).
- On shutdown, the producer flushes pending messages (up to 5 seconds).
- The internal delivery timeout (`message.timeout.ms`) is 30 seconds. If a message can't be delivered within that time, it's returned as an error and limpid's [queue retry](./README.md#queue-and-retry) handles re-delivery.

## Example

```
def output siem_kafka {
    type kafka
    brokers "kafka1:9092,kafka2:9092,kafka3:9092"
    topic "firewall-logs"
    compression lz4
    key source
}

def pipeline forward {
    input syslog_udp
    process parse_cef | {
        message = to_json()
    }
    output siem_kafka
}
```
