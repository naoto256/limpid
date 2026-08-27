# Performance

## Qualified v0.8.0 results

The release benchmark uses four canonical single-input, single-pipeline,
single-output configurations. Every output is a UDP sink reached through the
normal in-memory output queue.

| Workload | Pipeline shape | Post-merge median events/sec |
|---|---|---:|
| A | passthrough; no DSL process | 406,617.5 |
| B | `syslog.parse(ingress)` | 414,161.0 |
| C | `syslog.parse`, JSON parse, two regex extractions, and a conditional | 156,887.0 |
| D | `syslog.parse`, JSON parse, OCSF Authentication object composition, and `to_json` | 251,190.0 |

These are aggregate results from a benchmark slice allowed to run on CPUs
14–15 of a Linux aarch64 Parallels VM. They are not single-core results and
should not be treated as capacity guarantees for other machines or production
traffic.

## Methodology

- Rust 1.95 release builds were compared before and after the merged stage
  latency metrics change.
- Each observation processed 1,500,000 events with exact receive, finish, and
  write counts.
- Each build had 20 observations per workload. Alternating ABBA/BAAB block
  order limited drift bias.
- A byte-identical null comparison first qualified workloads A and B against
  the unchanged ±2% throughput-resolution criterion.
- The comparison used a deterministic 100,000-resample nonparametric bootstrap
  of block medians for its 95% confidence intervals.

The complete 95% interval stayed within ±2% for A, B, and D. Workload C had a
median change of -1.791% with a 95% interval of [-2.294%, -0.813%]. Because
that interval crosses the -2% boundary, the overall pre/post overhead gate is
**inconclusive**: it neither establishes a greater-than-2% regression nor
supports an "overhead below 2%" claim.

## Workload C diagnostic

A separate diagnostic profile was not used as gate evidence. It measured about
161.7 ns/event additional median task-clock time in C. Source inspection and
disassembly confirm the required dispatch clock sample and input histogram
work. The profile showed no evidence of a new allocator, lock, queue, or I/O
path. A code-layout effect around the regex-heavy path remains only a hypothesis
because cache performance counters were unavailable on the VM. The fixed
instrumentation cost was accepted and no code change was recommended.
