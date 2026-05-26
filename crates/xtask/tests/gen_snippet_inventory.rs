//! Integration test for the inventory rendering pipeline.
//!
//! Builds an in-memory set of parser headers (mimicking what
//! `header::parse` would produce on real `.limpid` files), runs
//! the inventory renderer, and asserts the output shape. This
//! locks the contract of `render_parsers_table` /
//! `replace_marked_region` without touching the real workspace
//! tree.
//!
//! End-to-end orchestrator testing (the `gen_snippet_inventory::run`
//! function, which reads from disk and writes the README) stays in
//! the live `cargo xtask gen-snippet-inventory --check` invocation
//! wired into CI. That is the more meaningful check — it catches
//! drift against the actual on-disk parsers.

// We re-use the xtask crate's internal modules; the crate is a bin
// (`xtask`) but `cargo test --package xtask` compiles `main.rs` as a
// library target for tests, so the modules below are reachable.

#[path = "../src/header.rs"]
#[allow(dead_code)]
mod header;

#[path = "../src/inventory.rs"]
#[allow(dead_code)]
mod inventory;

use std::collections::BTreeMap;
use std::path::PathBuf;

use header::ParserHeader;
use inventory::{parsers_marker, render_parsers_table, replace_marked_region};

fn mk(file: &str, vendor: &str, wire: &str) -> ParserHeader {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    keys.insert("Vendor".into(), vendor.into());
    keys.insert("Wire".into(), wire.into());
    ParserHeader {
        file: PathBuf::from(format!("packaging/snippets/parsers/{file}")),
        keys,
        observed_order: vec!["Vendor".into(), "Wire".into()],
    }
}

#[test]
fn renders_two_column_table_with_categories() {
    // parse_syslog is in CATEGORIES under "Transport" — the
    // renderer must place it under that section.
    let headers = vec![mk("parse_syslog.limpid", "RFC 3164 / 5424 syslog", "syslog wire")];
    let (table, warnings) = render_parsers_table(&headers);
    assert!(table.contains("| **Transport** |"));
    assert!(table.contains("`parsers/parse_syslog.limpid`"));
    assert!(table.contains("RFC 3164 / 5424 syslog"));
    // The renderer always emits a "CATEGORIES references X but
    // no file" warning for each parser in CATEGORIES that's not
    // in this test's input set — expected for a minimal-input
    // test. The point of this assertion is that no warning
    // refers to `parse_syslog` itself (i.e. it was located).
    for w in &warnings {
        assert!(!w.contains("parse_syslog"), "unexpected warning for parse_syslog: {w}");
    }
}

#[test]
fn uncategorised_section_emits_warning() {
    // `parse_brandnew` is not in the CATEGORIES map → falls into
    // the Uncategorised group, with a warning surfaced.
    let headers = vec![mk("parse_brandnew.limpid", "Brandnew", "JSON")];
    let (table, warnings) = render_parsers_table(&headers);
    assert!(table.contains("| **Uncategorised** |"));
    assert!(table.contains("`parsers/parse_brandnew.limpid`"));
    let uncategorised_warnings: Vec<&String> = warnings
        .iter()
        .filter(|w| w.contains("Uncategorised"))
        .collect();
    assert_eq!(
        uncategorised_warnings.len(),
        1,
        "expected exactly one Uncategorised warning; got {warnings:?}"
    );
}

#[test]
fn backtick_code_span_does_not_truncate_source() {
    // Regression: the Check Point syslog parser carries a
    // backtick-delimited literal `; ` in its Wire value. Before the
    // backtick fix, `first_clause` cut mid-span, leaving an
    // unclosed backtick at end of the row.
    let headers = vec![mk(
        "parse_checkpoint_syslog.limpid",
        "Check Point",
        "Junos-style SD with `:` (not `=`) between key and quoted value and `; ` between pairs",
    )];
    let (table, _) = render_parsers_table(&headers);
    let parsers_row = table
        .lines()
        .find(|l| l.contains("parse_checkpoint_syslog"))
        .expect("checkpoint row");
    // backticks must be balanced inside the cell
    let bt_count = parsers_row.matches('`').count();
    assert_eq!(
        bt_count % 2,
        0,
        "row has unbalanced backticks: {parsers_row}"
    );
}

#[test]
fn duplicate_source_in_same_category_gets_disambiguated() {
    // parse_zeek_default / soc / full all live under "OSS NDR" in
    // CATEGORIES and share Vendor+Wire prose. The renderer must
    // tag the rows with their distinctive stem suffix so a reader
    // can tell them apart.
    let v = "Zeek";
    let w = "Native Zeek JSON output";
    let headers = vec![
        mk("parse_zeek_default.limpid", v, w),
        mk("parse_zeek_soc.limpid", v, w),
        mk("parse_zeek_full.limpid", v, w),
    ];
    let (table, _) = render_parsers_table(&headers);
    let lines: Vec<&str> = table.lines().filter(|l| l.contains("zeek")).collect();
    assert_eq!(lines.len(), 3, "expected 3 zeek rows, got {lines:?}");
    // All three should mention "scope:" disambiguation now.
    for l in &lines {
        assert!(l.contains("scope:"), "row missing disambiguator: {l}");
    }
}

#[test]
fn duplicate_detection_sees_through_bold_markup() {
    // Regression: if the dup-detection map keyed on the
    // pre-strip-bold source while the emit step used the
    // post-strip-bold source, three identical rows would slip
    // through the disambiguator because the lookup wouldn't
    // match the count key. Bold-marked Wire values are the
    // realistic trigger (Zeek headers carry `**Native Zeek JSON
    // output**`).
    let v = "Zeek";
    let w = "**Native Zeek JSON output**";
    let headers = vec![
        mk("parse_zeek_default.limpid", v, w),
        mk("parse_zeek_soc.limpid", v, w),
        mk("parse_zeek_full.limpid", v, w),
    ];
    let (table, _) = render_parsers_table(&headers);
    let lines: Vec<&str> = table.lines().filter(|l| l.contains("zeek")).collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l.contains("scope:"), "row missing disambiguator: {l}");
        assert!(!l.contains("**"), "bold leaked into cell: {l}");
    }
}

#[test]
fn bold_markup_is_stripped_in_source_cells() {
    let headers = vec![mk(
        "parse_syslog.limpid",
        "**Boldface vendor name**",
        "wire",
    )];
    let (table, _) = render_parsers_table(&headers);
    let row = table.lines().find(|l| l.contains("syslog")).unwrap();
    assert!(!row.contains("**"), "bold markup leaked into cell: {row}");
    assert!(row.contains("Boldface vendor name"));
}

#[test]
fn replace_marked_region_round_trip() {
    let readme = "\
preamble

<!-- BEGIN: inventory:parsers -->
old hand-maintained table here
<!-- END: inventory:parsers -->

trailing prose
";
    let updated = replace_marked_region(readme, parsers_marker(), "NEW BLOCK").unwrap();
    assert!(updated.contains("<!-- BEGIN: inventory:parsers -->\nNEW BLOCK\n<!-- END: inventory:parsers -->"));
    assert!(updated.contains("preamble"));
    assert!(updated.contains("trailing prose"));
    assert!(!updated.contains("old hand-maintained"));
}
