use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

fn process_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn socket_path() -> (PathBuf, PathBuf) {
    let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("limpidctl-stats-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("control.sock");
    (dir, socket)
}

fn run_stats(response: &str, args: &[&str]) -> Output {
    let _process_guard = process_lock();
    let (dir, socket) = socket_path();
    let server_socket = socket.clone();
    let response = response.to_owned();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (child_started_tx, child_started_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let listener = match UnixListener::bind(&server_socket) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = ready_tx.send(Err(format!(
                    "failed to bind control socket {server_socket:?}: {error}"
                )));
                return;
            }
        };
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = ready_tx.send(Err(format!(
                "failed to configure control socket {server_socket:?}: {error}"
            )));
            return;
        }
        ready_tx
            .send(Ok(()))
            .expect("stats test caller dropped before server readiness");
        child_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("limpidctl child was not started within 2 seconds of server readiness");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept control connection: {error}"),
            }
        };
        let Some(ref mut stream) = stream else {
            return;
        };
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        assert_eq!(request, "stats\n");
        stream.write_all(response.as_bytes()).unwrap();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            server.join().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            panic!("stats test server setup failed: {error}");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            server.join().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            panic!("stats test server did not become ready within 2 seconds");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let panic = server.join().err();
            std::fs::remove_dir_all(dir).unwrap();
            panic!("stats test server disconnected before readiness: {panic:?}");
        }
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_limpidctl"));
    command.arg("--socket").arg(&socket).arg("stats");
    command.args(args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    child_started_tx
        .send(())
        .expect("stats test server stopped before limpidctl child startup");
    let output = child.wait_with_output().unwrap();

    server.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    output
}

fn counter_family(
    name: &str,
    label: &str,
    help: &str,
    values: &[(&str, u64)],
    reverse_series: bool,
) -> Value {
    let mut series: Vec<Value> = values
        .iter()
        .map(|(scope, value)| json!({"labels": {label: scope}, "value": value}))
        .collect();
    if reverse_series {
        series.reverse();
    }
    json!({"name": name, "type": "counter", "help": help, "series": series})
}

fn dropped_root_family(values: &[(&str, u64)], reverse_series: bool) -> Value {
    let mut series: Vec<Value> = values
        .iter()
        .map(|(pipeline, value)| {
            json!({
                "labels": {
                    "pipeline": pipeline,
                    "step": "0",
                    "process_path": "/",
                    "process_name": ""
                },
                "value": value
            })
        })
        .collect();
    if reverse_series {
        series.reverse();
    }
    json!({
        "name": "limpid_events_dropped_total",
        "type": "counter",
        "help": "Total events whose drop propagated through this processing node.",
        "series": series
    })
}

fn histogram_snapshot(buckets: Value) -> String {
    format!(
        "{}\n",
        json!({
            "schema": 1,
            "metrics": [{
                "name": "metric_histogram",
                "type": "histogram",
                "help": "Histogram validation fixture.",
                "series": [{
                    "labels": {"scope": "one"},
                    "buckets": buckets,
                    "sum": 3.5,
                    "count": 3
                }]
            }]
        })
    )
}

fn canonical_snapshot(reverse: bool) -> Value {
    let mut metrics = vec![
        counter_family(
            "limpid_pipeline_events_received_total",
            "pipeline",
            "received",
            &[
                ("unwritable_only", 30),
                ("compact", 10),
                ("errored_only", 20),
            ],
            reverse,
        ),
        counter_family(
            "limpid_pipeline_events_finished_total",
            "pipeline",
            "finished",
            &[
                ("unwritable_only", 29),
                ("compact", 9),
                ("errored_only", 19),
            ],
            false,
        ),
        dropped_root_family(
            &[("unwritable_only", 0), ("compact", 1), ("errored_only", 0)],
            false,
        ),
        counter_family(
            "limpid_pipeline_events_discarded_total",
            "pipeline",
            "discarded",
            &[("unwritable_only", 1), ("compact", 0), ("errored_only", 1)],
            false,
        ),
        counter_family(
            "limpid_pipeline_events_errored_total",
            "pipeline",
            "errored",
            &[("unwritable_only", 0), ("compact", 0), ("errored_only", 4)],
            false,
        ),
        counter_family(
            "limpid_pipeline_events_errored_unwritable_total",
            "pipeline",
            "unwritable",
            &[("unwritable_only", 3), ("compact", 0), ("errored_only", 0)],
            false,
        ),
        counter_family(
            "limpid_input_events_received_total",
            "input",
            "received",
            &[("zeta", 210), ("alpha", 110)],
            false,
        ),
        counter_family(
            "limpid_input_events_invalid_total",
            "input",
            "invalid",
            &[("zeta", 3), ("alpha", 2)],
            false,
        ),
        counter_family(
            "limpid_input_events_injected_total",
            "input",
            "injected",
            &[("zeta", 4), ("alpha", 3)],
            false,
        ),
        counter_family(
            "limpid_output_events_received_total",
            "output",
            "received",
            &[
                ("unwritable_only", 130),
                ("compact", 110),
                ("wedged_only", 120),
            ],
            false,
        ),
        counter_family(
            "limpid_output_events_injected_total",
            "output",
            "injected",
            &[("unwritable_only", 3), ("compact", 1), ("wedged_only", 2)],
            false,
        ),
        counter_family(
            "limpid_output_events_written_total",
            "output",
            "written",
            &[
                ("unwritable_only", 125),
                ("compact", 105),
                ("wedged_only", 115),
            ],
            false,
        ),
        counter_family(
            "limpid_output_events_failed_total",
            "output",
            "failed",
            &[("unwritable_only", 2), ("compact", 2), ("wedged_only", 2)],
            false,
        ),
        counter_family(
            "limpid_output_retries_total",
            "output",
            "retries",
            &[("unwritable_only", 5), ("compact", 3), ("wedged_only", 4)],
            false,
        ),
        counter_family(
            "limpid_output_events_wedged_total",
            "output",
            "wedged",
            &[("unwritable_only", 0), ("compact", 0), ("wedged_only", 2)],
            false,
        ),
        counter_family(
            "limpid_output_events_errored_unwritable_total",
            "output",
            "unwritable",
            &[("unwritable_only", 1), ("compact", 0), ("wedged_only", 0)],
            false,
        ),
    ];
    if reverse {
        metrics.reverse();
    }
    json!({"schema": 1, "metrics": metrics})
}

fn expected_default_table() -> String {
    format!(
        "Pipelines:\n\
         {compact_pipeline}\n\
         {errored_pipeline}\n\
         {unwritable_pipeline}\n\
         \nInputs:\n\
         {alpha_input}\n\
         {zeta_input}\n\
         \nOutputs:\n\
         {compact_output}\n\
         {unwritable_output}\n\
         {wedged_output}\n",
        compact_pipeline = format_args!(
            "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded",
            "compact", 10, 9, 1, 0
        ),
        errored_pipeline = format_args!(
            "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded  {:>8} errored",
            "errored_only", 20, 19, 0, 1, 4
        ),
        unwritable_pipeline = format_args!(
            "  {:<24} {:>8} received  {:>8} finished  {:>8} dropped  {:>8} discarded  {:>8} errored  {:>8} errored_unwritable",
            "unwritable_only", 30, 29, 0, 1, 0, 3
        ),
        alpha_input = format_args!(
            "  {:<24} {:>8} received  {:>8} invalid  {:>8} injected",
            "alpha", 110, 2, 3
        ),
        zeta_input = format_args!(
            "  {:<24} {:>8} received  {:>8} invalid  {:>8} injected",
            "zeta", 210, 3, 4
        ),
        compact_output = format_args!(
            "  {:<24} {:>8} received  {:>8} injected  {:>8} written  {:>8} failed  {:>8} retries",
            "compact", 110, 1, 105, 2, 3
        ),
        unwritable_output = format_args!(
            "  {:<24} {:>8} received  {:>8} injected  {:>8} written  {:>8} failed  {:>8} retries  {:>8} errored_unwritable",
            "unwritable_only", 130, 3, 125, 2, 5, 1
        ),
        wedged_output = format_args!(
            "  {:<24} {:>8} received  {:>8} injected  {:>8} written  {:>8} failed  {:>8} retries  {:>8} wedged",
            "wedged_only", 120, 2, 115, 2, 4, 2
        ),
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn default_stats_maps_schema_v1_to_the_existing_sorted_table() {
    let expected = expected_default_table();
    for reverse in [false, true] {
        let response = format!("{}\n", canonical_snapshot(reverse));
        let output = run_stats(&response, &[]);
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(stdout(&output), expected);
    }
}

fn process_snapshot() -> Value {
    let mut payload = canonical_snapshot(false);
    for (name, root_value, leaf_value) in [
        ("limpid_process_events_in_total", 4, 3),
        ("limpid_process_events_out_total", 4, 1),
        ("limpid_process_events_errored_total", 0, 1),
    ] {
        payload["metrics"].as_array_mut().unwrap().push(json!({
            "name": name,
            "type": "counter",
            "help": "Process invocation fixture.",
            "series": [
                {
                    "labels": {
                        "pipeline": "compact",
                        "step": "10",
                        "process_path": "/dispatch/leaf",
                        "process_name": "leaf"
                    },
                    "value": leaf_value
                },
                {
                    "labels": {
                        "pipeline": "compact",
                        "step": "2",
                        "process_path": "/dispatch",
                        "process_name": "dispatch"
                    },
                    "value": root_value
                }
            ]
        }));
    }
    family_mut(&mut payload, "limpid_events_dropped_total")["series"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({
                "labels": {
                    "pipeline": "compact",
                    "step": "10",
                    "process_path": "/dispatch/leaf",
                    "process_name": "leaf"
                },
                "value": 1
            }),
            json!({
                "labels": {
                    "pipeline": "compact",
                    "step": "2",
                    "process_path": "/dispatch",
                    "process_name": "dispatch"
                },
                "value": 0
            }),
        ]);
    payload
}

fn family_mut<'a>(payload: &'a mut Value, name: &str) -> &'a mut Value {
    payload["metrics"]
        .as_array_mut()
        .expect("metrics")
        .iter_mut()
        .find(|family| family["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn for_each_process_family_mut(payload: &mut Value, mut update: impl FnMut(&mut Value)) {
    for name in [
        "limpid_process_events_in_total",
        "limpid_process_events_out_total",
        "limpid_events_dropped_total",
        "limpid_process_events_errored_total",
    ] {
        update(family_mut(payload, name));
    }
}

fn first_process_series_mut(family: &mut Value) -> &mut Value {
    family["series"]
        .as_array_mut()
        .expect("series")
        .iter_mut()
        .find(|series| series["labels"]["process_path"] != "/")
        .expect("process series")
}

fn process_default_validator_only_defects() -> Vec<(&'static str, Value)> {
    let mut cases = Vec::new();

    let mut missing_label = process_snapshot();
    for_each_process_family_mut(&mut missing_label, |family| {
        first_process_series_mut(family)["labels"]
            .as_object_mut()
            .unwrap()
            .remove("process_name");
    });
    cases.push(("missing label", missing_label));

    let mut extra_label = process_snapshot();
    for_each_process_family_mut(&mut extra_label, |family| {
        first_process_series_mut(family)["labels"]
            .as_object_mut()
            .unwrap()
            .insert("extra".to_owned(), json!("x"));
    });
    cases.push(("extra label", extra_label));

    let mut non_numeric_step = process_snapshot();
    for_each_process_family_mut(&mut non_numeric_step, |family| {
        first_process_series_mut(family)["labels"]["step"] = json!("ten");
    });
    cases.push(("non-numeric step", non_numeric_step));

    cases
}

fn line_tokens(text: &str, token: &str) -> Vec<String> {
    text.lines()
        .find(|line| line.split_whitespace().any(|part| part == token))
        .unwrap_or_else(|| panic!("missing line containing {token:?} in {text:?}"))
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[test]
fn process_counters_render_a_numerically_sorted_default_invocation_table() {
    let payload = process_snapshot();
    let response = format!("{payload}\n");

    let default = run_stats(&response, &[]);
    assert!(default.status.success(), "{default:?}");
    let default = stdout(&default);
    assert!(default.starts_with(&expected_default_table()));
    let process_header = default
        .lines()
        .find(|line| line.trim() == "Processes:")
        .expect("default output must include the process section")
        .split_whitespace()
        .collect::<Vec<_>>();
    assert_eq!(process_header, ["Processes:"]);
    let root_row = default
        .lines()
        .find(|line| line.contains("/dispatch "))
        .expect("root process row");
    assert_eq!(
        root_row,
        format!(
            "  {:<16} {:>4}  {:<32} {:<16} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored",
            "compact", 2, "/dispatch", "dispatch", 4, 4, 0, 0
        )
    );
    let leaf_row = default
        .lines()
        .find(|line| line.contains("/dispatch/leaf "))
        .expect("leaf process row");
    assert_eq!(
        leaf_row,
        format!(
            "  {:<16} {:>4}  {:<32} {:<16} {:>8} in  {:>8} out  {:>8} dropped  {:>8} errored",
            "compact", 10, "/dispatch/leaf", "leaf", 3, 1, 1, 1
        )
    );
    assert_eq!(
        line_tokens(&default, "/dispatch"),
        [
            "compact",
            "2",
            "/dispatch",
            "dispatch",
            "4",
            "in",
            "4",
            "out",
            "0",
            "dropped",
            "0",
            "errored",
        ]
    );
    assert_eq!(
        line_tokens(&default, "/dispatch/leaf"),
        [
            "compact",
            "10",
            "/dispatch/leaf",
            "leaf",
            "3",
            "in",
            "1",
            "out",
            "1",
            "dropped",
            "1",
            "errored",
        ]
    );
    let root = default.find("/dispatch ").expect("root row");
    let leaf = default.find("/dispatch/leaf ").expect("leaf row");
    assert!(root < leaf, "step 2 must sort before step 10 numerically");

    let details = run_stats(&response, &["--details"]);
    assert!(details.status.success(), "{details:?}");
    let details = stdout(&details);
    for name in [
        "limpid_process_events_in_total",
        "limpid_process_events_out_total",
        "limpid_events_dropped_total",
        "limpid_process_events_errored_total",
    ] {
        assert!(details.contains(name), "missing {name}");
    }
    assert!(details.contains(
        r#"pipeline="compact", process_name="leaf", process_path="/dispatch/leaf", step="10""#
    ));

    let json = run_stats(&response, &["--json"]);
    assert!(json.status.success(), "{json:?}");
    assert_eq!(json.stdout, response.as_bytes());
}

#[test]
fn malformed_process_families_fall_back_to_the_whole_raw_response() {
    let mut cases = Vec::new();

    let mut missing_family = process_snapshot();
    missing_family["metrics"]
        .as_array_mut()
        .unwrap()
        .retain(|family| family["name"] != "limpid_process_events_errored_total");
    cases.push(("missing family", missing_family));

    let mut duplicate_series = process_snapshot();
    let duplicate = duplicate_series["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|family| family["name"] == "limpid_process_events_in_total")
        .unwrap()["series"][0]
        .clone();
    family_mut(&mut duplicate_series, "limpid_process_events_in_total")["series"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    cases.push(("duplicate exact series", duplicate_series));

    let mut wrong_type = process_snapshot();
    family_mut(&mut wrong_type, "limpid_process_events_out_total")["type"] = json!("gauge");
    cases.push(("wrong family type", wrong_type));

    cases.extend(process_default_validator_only_defects());

    let mut mismatched_identity = process_snapshot();
    family_mut(&mut mismatched_identity, "limpid_process_events_out_total")["series"][0]["labels"]
        ["process_path"] = json!("/dispatch/other");
    cases.push(("mismatched identities", mismatched_identity));

    let mut invalid_value = process_snapshot();
    family_mut(&mut invalid_value, "limpid_process_events_in_total")["series"][0]["value"] =
        json!("3");
    cases.push(("invalid value", invalid_value));

    let mut did_not_fall_back = Vec::new();
    for (name, payload) in cases {
        let response = format!("{payload}\n");
        let output = run_stats(&response, &[]);
        assert!(output.status.success(), "{name}: {output:?}");
        if output.stdout != response.as_bytes() {
            did_not_fall_back.push(name);
        }
    }
    assert!(
        did_not_fall_back.is_empty(),
        "each single defect must cause a whole raw fallback; rendered: {did_not_fall_back:?}"
    );
}

#[test]
fn malformed_dropped_hierarchy_roots_fall_back_to_the_raw_response() {
    let mut wrong_step = process_snapshot();
    family_mut(&mut wrong_step, "limpid_events_dropped_total")["series"][0]["labels"]["step"] =
        json!("1");

    let mut nonempty_name = process_snapshot();
    family_mut(&mut nonempty_name, "limpid_events_dropped_total")["series"][0]["labels"]["process_name"] =
        json!("pipeline");

    let mut missing_root = process_snapshot();
    family_mut(&mut missing_root, "limpid_events_dropped_total")["series"]
        .as_array_mut()
        .unwrap()
        .retain(|series| series["labels"]["process_path"] != "/");

    for (name, payload) in [
        ("wrong root step", wrong_step),
        ("non-empty root process name", nonempty_name),
        ("missing root", missing_root),
    ] {
        let response = format!("{payload}\n");
        for args in [&[][..], &["--details"][..]] {
            let output = run_stats(&response, args);
            assert!(output.status.success(), "{name}: {output:?}");
            assert_eq!(output.stdout, response.as_bytes(), "{name}: {args:?}");
        }
    }
}

#[test]
fn details_remains_generic_for_process_default_validator_only_defects() {
    for (name, payload) in process_default_validator_only_defects() {
        let response = format!("{payload}\n");
        let output = run_stats(&response, &["--details"]);
        assert!(output.status.success(), "{name}: {output:?}");
        assert_ne!(
            output.stdout,
            response.as_bytes(),
            "{name} is DTO-valid and must remain generic in details mode"
        );
        assert!(stdout(&output).contains("limpid_process_events_in_total"));
    }
}

#[test]
fn details_remains_generic_when_process_families_are_incomplete() {
    let mut payload = process_snapshot();
    payload["metrics"]
        .as_array_mut()
        .unwrap()
        .retain(|family| family["name"] != "limpid_process_events_errored_total");
    let response = format!("{payload}\n");

    let output = run_stats(&response, &["--details"]);
    assert!(output.status.success(), "{output:?}");
    assert_ne!(output.stdout, response.as_bytes());
    assert!(stdout(&output).contains("limpid_process_events_in_total"));
}

#[test]
fn json_mode_preserves_the_complete_control_response() {
    let response = concat!(
        "{\"schema\":1,\"metrics\":[",
        "{\"name\":\"custom_gauge\",\"type\":\"gauge\",\"help\":\"all fields\",",
        "\"series\":[{\"labels\":{\"scope\":\"one\"},\"value\":7}]}",
        "]}\n"
    );
    let output = run_stats(response, &["--json"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout == response.as_bytes(), "raw JSON was changed");
}

#[test]
fn details_renders_all_types_with_deterministic_order_and_complete_fields() {
    let payload = json!({
        "schema": 1,
        "metrics": [
            {
                "name": "metric_zeta",
                "type": "histogram",
                "help": "Latency distribution.",
                "series": [{
                    "labels": {"route": "west"},
                    "buckets": [[0.125, 17], [0.875, 29]],
                    "sum": 13.75,
                    "count": 31
                }]
            },
            {
                "name": "metric_alpha",
                "type": "counter",
                "help": "Accepted events.",
                "series": [
                    {"labels": {"a": "two", "z": "first"}, "value": 2},
                    {"labels": {"a": "one", "z": "last"}, "value": 1}
                ]
            },
            {
                "name": "metric_middle",
                "type": "gauge",
                "help": "Current depth.",
                "series": [{
                    "labels": {"env": "prod\"east\\one\nline"},
                    "value": 9
                }]
            }
        ]
    });
    let response = format!("{payload}\n");
    let output = run_stats(&response, &["--details"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout(&output);

    let counter = text.find("metric_alpha").unwrap();
    let gauge = text.find("metric_middle").unwrap();
    let histogram = text.find("metric_zeta").unwrap();
    assert!(counter < gauge && gauge < histogram);
    let counter_block = &text[counter..gauge];
    let gauge_block = &text[gauge..histogram];
    let histogram_block = &text[histogram..];
    assert!(counter_block.contains("counter"));
    assert!(counter_block.contains("Accepted events."));
    assert!(text.find("a=\"one\"").unwrap() < text.find("a=\"two\"").unwrap());
    assert!(counter_block.contains(
        r#"  labels: a="one", z="last"
    value: 1
"#
    ));
    assert!(counter_block.contains(
        r#"  labels: a="two", z="first"
    value: 2
"#
    ));
    assert!(gauge_block.contains("gauge"));
    assert!(gauge_block.contains("Current depth."));
    assert!(gauge_block.contains(
        r#"  labels: env="prod\"east\\one\nline"
    value: 9
"#
    ));
    assert!(histogram_block.contains("histogram"));
    assert!(histogram_block.contains("Latency distribution."));
    assert!(histogram_block.contains("route=\"west\""));
    assert!(histogram_block.contains("    buckets: 0.125 => 17 0.875 => 29\n"));
    assert!(histogram_block.contains("    sum: 13.75\n"));
    assert!(histogram_block.contains("    count: 31\n"));
    assert!(!histogram_block.contains("+Inf"));
}

#[test]
fn details_rejects_invalid_histogram_sequences_without_repair() {
    for buckets in [
        json!([]),
        json!([[0.5, 1]]),
        json!([[0.125, 1], [0.875, 1], [2.0, 3]]),
        // Bucket and total atomics are loaded independently, so observe ordering
        // permits a transient last-bucket count greater than the total count.
        json!([[0.125, 1], [0.875, 2], [2.0, 4]]),
    ] {
        let response = histogram_snapshot(buckets);
        let output = run_stats(&response, &["--details"]);
        assert!(output.status.success(), "{:?}", output);
        assert_ne!(output.stdout, response.as_bytes());
        assert!(stdout(&output).contains("metric_histogram"));
    }

    for buckets in [
        json!([[1.0, 1], [1.0, 2]]),
        json!([[2.0, 1], [1.0, 2]]),
        json!([[1.0, 2], [2.0, 1]]),
    ] {
        let response = histogram_snapshot(buckets);
        let output = run_stats(&response, &["--details"]);
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(output.stdout, response.as_bytes());
    }
}

#[test]
fn default_ignores_unknown_families_but_details_includes_them() {
    let mut payload = canonical_snapshot(false);
    payload["metrics"].as_array_mut().unwrap().push(json!({
        "name": "future_metric",
        "type": "gauge",
        "help": "Future metric.",
        "series": [{"labels": {"scope": "future"}, "value": 42}]
    }));
    let response = format!("{payload}\n");

    let default_output = run_stats(&response, &[]);
    assert!(default_output.status.success(), "{:?}", default_output);
    assert_eq!(stdout(&default_output), expected_default_table());
    assert!(!stdout(&default_output).contains("future_metric"));

    let details_output = run_stats(&response, &["--details"]);
    assert!(details_output.status.success(), "{:?}", details_output);
    assert!(stdout(&details_output).contains("future_metric"));
    assert!(stdout(&details_output).contains("scope=\"future\""));
    assert!(stdout(&details_output).contains("42"));
}

#[test]
fn build_info_is_generic_in_details_and_does_not_change_default_or_raw_json() {
    let mut payload = canonical_snapshot(false);
    payload["metrics"].as_array_mut().unwrap().push(json!({
        "name": "limpid_build_info",
        "type": "gauge",
        "help": "Build information for the running limpid node.",
        "series": [{
            "labels": {"node_id": "edge-a", "version": "0.7.15"},
            "value": 1
        }]
    }));
    let response = format!("{payload}\n");

    let default_output = run_stats(&response, &[]);
    assert!(default_output.status.success(), "{default_output:?}");
    assert_eq!(stdout(&default_output), expected_default_table());
    assert!(!stdout(&default_output).contains("limpid_build_info"));

    let details_output = run_stats(&response, &["--details"]);
    assert!(details_output.status.success(), "{details_output:?}");
    let details = stdout(&details_output);
    assert!(details.contains("limpid_build_info"));
    assert!(details.contains("gauge"));
    assert!(details.contains("Build information for the running limpid node."));
    assert!(details.contains("node_id=\"edge-a\""));
    assert!(details.contains("version=\"0.7.15\""));
    assert!(details.contains("value: 1"));

    let json_output = run_stats(&response, &["--json"]);
    assert!(json_output.status.success(), "{json_output:?}");
    assert_eq!(json_output.stdout, response.as_bytes());
}

#[test]
fn malformed_known_families_fall_back_to_the_raw_response() {
    let canonical = canonical_snapshot(false);
    let mut cases = Vec::new();

    let mut missing = canonical.clone();
    missing["metrics"].as_array_mut().unwrap().remove(0);
    cases.push(missing);

    let mut duplicate = canonical.clone();
    let duplicate_family = duplicate["metrics"][0].clone();
    duplicate["metrics"]
        .as_array_mut()
        .unwrap()
        .push(duplicate_family);
    cases.push(duplicate);

    let mut wrong_type = canonical.clone();
    wrong_type["metrics"][0]["type"] = json!("gauge");
    cases.push(wrong_type);

    let mut wrong_label = canonical.clone();
    wrong_label["metrics"][0]["series"][0]["labels"] = json!({"wrong": "zeta"});
    cases.push(wrong_label);

    let mut extra_label = canonical.clone();
    extra_label["metrics"][0]["series"][0]["labels"] =
        json!({"pipeline": "unwritable_only", "extra": "x"});
    cases.push(extra_label);

    let mut wrong_value = canonical;
    wrong_value["metrics"][0]["series"][0]["value"] = json!("30");
    cases.push(wrong_value);

    for (index, payload) in cases.into_iter().enumerate() {
        let response = format!("{payload}\n");
        let output = run_stats(&response, &[]);
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout == response.as_bytes(),
            "malformed known family did not use raw fallback"
        );
        if index == 0 {
            let details = run_stats(&response, &["--details"]);
            assert!(
                details.status.success(),
                "stderr={}",
                String::from_utf8_lossy(&details.stderr)
            );
            assert!(
                details.stdout == response.as_bytes(),
                "malformed known family did not use details raw fallback"
            );
        }
    }
}

#[test]
fn invalid_and_unsupported_responses_preserve_raw_fallback() {
    for response in [
        "not json at all\n",
        "{\"schema\":2,\"metrics\":[]}\n",
        "{\"schema\":1,\"metrics\":\"wrong\"}\n",
    ] {
        for args in [&[][..], &["--details"][..]] {
            let output = run_stats(response, args);
            assert!(
                output.status.success(),
                "stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stdout == response.as_bytes(),
                "invalid or unsupported response did not use raw fallback"
            );
        }
    }
}

#[test]
fn json_and_details_are_mutually_exclusive() {
    let _process_guard = process_lock();
    let output = Command::new(env!("CARGO_BIN_EXE_limpidctl"))
        .args(["stats", "--json", "--details"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--json"));
    assert!(stderr.contains("--details"));
    assert!(stderr.contains("cannot be used with"));
}
