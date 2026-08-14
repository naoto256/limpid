use limpid_metrics_schema::{
    DROPPED_ROOT_PROCESS_NAME, DROPPED_ROOT_STEP, EVENTS_DROPPED_OWN_TOTAL, EVENTS_DROPPED_TOTAL,
    MetricsSnapshot, PROCESS_EVENTS_ERRORED_TOTAL, PROCESS_EVENTS_IN_TOTAL,
    PROCESS_EVENTS_OUT_TOTAL, PROCESS_LABEL_NAME, PROCESS_LABEL_PATH, PROCESS_LABEL_PIPELINE,
    PROCESS_LABEL_STEP, PROCESS_PATH_ROOT, PROCESS_PATH_SEPARATOR, is_direct_child,
    process_path_leaf, process_path_parent,
};
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("fixtures/schema-v1.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn assert_rejected(mut mutate: impl FnMut(&mut Value)) {
    let mut candidate = fixture();
    mutate(&mut candidate);
    assert!(serde_json::from_value::<MetricsSnapshot>(candidate).is_err());
}

#[test]
fn schema_v1_fixture_round_trips_through_the_wire_dto() {
    let snapshot: MetricsSnapshot = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(), fixture());
}

#[test]
fn serialization_preserves_the_legacy_object_member_order() {
    let snapshot: MetricsSnapshot = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        r#"{"schema":1,"metrics":[{"name":"requests_total","type":"counter","help":"Requests.","series":[{"labels":{"route":"west"},"value":7}]},{"name":"queue_depth","type":"gauge","help":"Queue depth.","series":[{"labels":{"output":"sink"},"value":3}]},{"name":"latency_seconds","type":"histogram","help":"Latency.","series":[{"labels":{"route":"west"},"buckets":[[0.5,2],[1.0,3]],"sum":1.75,"count":4}]}]}"#
    );
}

#[test]
fn every_wire_level_tolerates_additive_unknown_fields() {
    let mut mixed = fixture();
    mixed["future_root"] = json!({"version": 2});
    mixed["metrics"][0]["future_family"] = json!(["ignored"]);
    mixed["metrics"][0]["series"][0]["future_value_series"] = json!({"ignored": true});
    mixed["metrics"][2]["series"][0]["future_histogram_series"] = json!("ignored");

    let parsed: MetricsSnapshot = serde_json::from_value(mixed).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), fixture());
}

#[test]
fn family_type_selects_the_series_shape_without_sibling_field_confusion() {
    let mut counter_with_histogram_siblings = fixture();
    let value = &mut counter_with_histogram_siblings["metrics"][0]["series"][0];
    value["buckets"] = json!([[0.5, 1]]);
    value["sum"] = json!(0.5);
    value["count"] = json!(1);
    let parsed: MetricsSnapshot = serde_json::from_value(counter_with_histogram_siblings).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), fixture());

    let mut histogram_with_value_sibling = fixture();
    histogram_with_value_sibling["metrics"][2]["series"][0]["value"] = json!(99);
    let parsed: MetricsSnapshot = serde_json::from_value(histogram_with_value_sibling).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), fixture());
}

#[test]
fn family_type_rejects_wrong_or_mixed_series_shapes() {
    assert_rejected(|root| {
        let histogram = root["metrics"][2]["series"].clone();
        root["metrics"][0]["series"] = histogram;
    });
    assert_rejected(|root| {
        let histogram = root["metrics"][2]["series"].clone();
        root["metrics"][1]["series"] = histogram;
    });
    assert_rejected(|root| {
        let value = root["metrics"][0]["series"].clone();
        root["metrics"][2]["series"] = value;
    });
    assert_rejected(|root| {
        let histogram = root["metrics"][2]["series"][0].clone();
        root["metrics"][0]["series"]
            .as_array_mut()
            .unwrap()
            .push(histogram);
    });
    assert_rejected(|root| {
        let histogram = root["metrics"][2]["series"][0].clone();
        root["metrics"][1]["series"]
            .as_array_mut()
            .unwrap()
            .push(histogram);
    });
    assert_rejected(|root| {
        let value = root["metrics"][0]["series"][0].clone();
        root["metrics"][2]["series"]
            .as_array_mut()
            .unwrap()
            .push(value);
    });
}

#[test]
fn every_required_wire_field_is_required_independently() {
    assert_rejected(|root| {
        root.as_object_mut().unwrap().remove("schema");
    });
    assert_rejected(|root| {
        root.as_object_mut().unwrap().remove("metrics");
    });
    for field in ["name", "type", "help", "series"] {
        assert_rejected(|root| {
            root["metrics"][0].as_object_mut().unwrap().remove(field);
        });
    }
    for field in ["labels", "value"] {
        assert_rejected(|root| {
            root["metrics"][0]["series"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
        });
    }
    for field in ["labels", "buckets", "sum", "count"] {
        assert_rejected(|root| {
            root["metrics"][2]["series"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
        });
    }
}

#[test]
fn wire_scalar_and_collection_types_are_strict() {
    assert_rejected(|root| root["schema"] = json!(1.0));
    assert_rejected(|root| root["metrics"] = json!({}));
    assert_rejected(|root| root["metrics"][0]["name"] = json!(false));
    assert_rejected(|root| root["metrics"][0]["type"] = json!(1));
    assert_rejected(|root| root["metrics"][0]["help"] = json!([]));
    assert_rejected(|root| root["metrics"][0]["series"] = json!({}));
    assert_rejected(|root| root["metrics"][0]["series"][0]["labels"] = json!([]));
    assert_rejected(|root| root["metrics"][0]["series"][0]["labels"]["route"] = json!(1));
    assert_rejected(|root| root["metrics"][0]["series"][0]["value"] = json!(true));
    assert_rejected(|root| root["metrics"][2]["series"][0]["buckets"] = json!({}));
    assert_rejected(|root| root["metrics"][2]["series"][0]["buckets"][0][0] = json!("0.5"));
    assert_rejected(|root| root["metrics"][2]["series"][0]["buckets"][0][1] = json!(1.5));
    assert_rejected(|root| root["metrics"][2]["series"][0]["sum"] = json!("1.75"));
    assert_rejected(|root| root["metrics"][2]["series"][0]["count"] = json!(false));
}

#[test]
fn integer_and_float_edge_representations_are_rejected() {
    let overflow: Value = serde_json::from_str("18446744073709551616").unwrap();
    for invalid in [json!(-1), json!(1.5), overflow] {
        assert_rejected(|root| root["metrics"][0]["series"][0]["value"] = invalid.clone());
    }
    for invalid in [json!(-1), json!(1.5)] {
        assert_rejected(|root| root["metrics"][2]["series"][0]["buckets"][0][1] = invalid.clone());
        assert_rejected(|root| root["metrics"][2]["series"][0]["count"] = invalid.clone());
    }
    assert_rejected(|root| {
        root["metrics"][2]["series"][0]["buckets"][0][1] =
            serde_json::from_str("18446744073709551616").unwrap()
    });
    assert_rejected(|root| {
        root["metrics"][2]["series"][0]["count"] =
            serde_json::from_str("18446744073709551616").unwrap()
    });
    assert_rejected(|root| root["metrics"][2]["series"][0]["buckets"][0][0] = Value::Null);
    assert_rejected(|root| root["metrics"][2]["series"][0]["sum"] = Value::Null);
}

#[test]
fn schema_uses_the_full_u32_wire_domain_without_enforcing_version_semantics() {
    for invalid in [
        json!(-1),
        json!(1.5),
        Value::Null,
        json!(u64::from(u32::MAX) + 1),
    ] {
        assert_rejected(|root| root["schema"] = invalid.clone());
    }

    let mut maximum = fixture();
    maximum["schema"] = json!(u32::MAX);
    assert!(serde_json::from_value::<MetricsSnapshot>(maximum).is_ok());
}

#[test]
fn an_empty_histogram_bucket_list_is_valid() {
    let mut candidate = fixture();
    candidate["metrics"][2]["series"][0]["buckets"] = json!([]);
    assert!(serde_json::from_value::<MetricsSnapshot>(candidate).is_ok());
}

#[test]
fn dropped_hierarchy_and_process_metric_names_are_the_well_known_schema_v1_vocabulary() {
    assert_eq!(
        [
            PROCESS_EVENTS_IN_TOTAL,
            PROCESS_EVENTS_OUT_TOTAL,
            EVENTS_DROPPED_TOTAL,
            PROCESS_EVENTS_ERRORED_TOTAL,
            EVENTS_DROPPED_OWN_TOTAL,
        ],
        [
            "limpid_process_events_in_total",
            "limpid_process_events_out_total",
            "limpid_events_dropped_total",
            "limpid_process_events_errored_total",
            "limpid_events_dropped_own_total",
        ]
    );
    assert_eq!(
        [
            PROCESS_LABEL_PIPELINE,
            PROCESS_LABEL_STEP,
            PROCESS_LABEL_PATH,
            PROCESS_LABEL_NAME,
        ],
        ["pipeline", "step", "process_path", "process_name"]
    );
    assert_eq!(PROCESS_PATH_SEPARATOR, '/');
    assert_eq!(PROCESS_PATH_ROOT, "/");
    assert_eq!(DROPPED_ROOT_STEP, "0");
    assert_eq!(DROPPED_ROOT_PROCESS_NAME, "");
}

#[test]
fn process_path_leaf_and_parent_follow_the_rooted_invocation_hierarchy() {
    let cases = [
        ("/dispatch/leaf", Some("leaf"), Some("/dispatch")),
        ("/dispatch", Some("dispatch"), Some("/")),
        ("/(inline)", Some("(inline)"), Some("/")),
        ("/", None, None),
        ("", None, None),
        ("dispatch", None, None),
        ("/dispatch/", None, None),
        ("/dispatch//leaf", None, None),
    ];

    for (path, leaf, parent) in cases {
        assert_eq!(process_path_leaf(path), leaf, "leaf for {path:?}");
        assert_eq!(process_path_parent(path), parent, "parent for {path:?}");
    }
}

#[test]
fn direct_child_requires_exactly_one_valid_path_segment() {
    for (parent, child) in [
        ("/", "/dispatch"),
        ("/dispatch", "/dispatch/leaf"),
        ("/dispatch", "/dispatch/(inline)"),
    ] {
        assert!(is_direct_child(parent, child), "{parent:?} -> {child:?}");
    }

    for (parent, child) in [
        ("/", "/dispatch/leaf"),
        ("/dispatch", "/dispatch"),
        ("/dispatch", "/dispatcher/leaf"),
        ("/dispatch", "/dispatch/leaf/grandchild"),
        ("/dispatch", "/dispatch/"),
        ("/dispatch", "/dispatch//leaf"),
        ("dispatch", "dispatch/leaf"),
    ] {
        assert!(
            !is_direct_child(parent, child),
            "{parent:?} must not own {child:?}"
        );
    }
}
