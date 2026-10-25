# Performance

## Qualified v0.8.0 results

The release benchmark uses four canonical single-input, single-pipeline,
single-output configurations. Every output is a UDP sink reached through the
normal in-memory output queue.

| Workload | Pipeline shape | Median events/sec |
|---|---|---:|
| A | passthrough; no DSL process | 406,617.5 |
| B | `syslog.parse(ingress)` | 414,161.0 |
| C | `syslog.parse`, JSON parse, two regex extractions, and a conditional | 156,887.0 |
| D | `syslog.parse`, JSON parse, OCSF Authentication object composition, and `to_json` | 251,190.0 |

These are aggregate v0.8.0 measurements on Linux aarch64, limited to two CPUs. They are not single-core results and
should not be treated as capacity guarantees for other machines or production
traffic.

Comparing builds before and after adding stage-latency metrics, workloads
A, B, and D stayed within ±2%. Workload C had a 95% confidence interval spanning
-2.294% to -0.813%. Because it crosses the -2% boundary, the comparison is
inconclusive: it supports neither a greater-than-2% regression nor a claim of
overhead below 2%.
