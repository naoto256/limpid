//! Schema validation for top-level global blocks (`control`, `geoip`,
//! `table`) that aren't module properties but are still part of
//! `--check`'s loud-property-typo coverage.
//!
//! Each block's schema lives next to this file rather than next to
//! the runtime that consumes it: there is one runtime path per block
//! (in `runtime.rs`/`functions::geoip`/`functions::table`) and the
//! schema is small, so co-locating the per-block specs with the
//! validator avoids fanning the dependency across three subsystems.

use crate::dsl::ast::Property;
use crate::dsl::schema::{self as ds, PropertySpec, PropertyValueKind};
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic};

const CONTROL_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "socket",
        required: false,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "error_log",
        required: false,
        kind: PropertyValueKind::String,
    },
];

const GEOIP_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "database",
    required: false,
    kind: PropertyValueKind::String,
}];

/// Schema for a single named entry inside the top-level `table {
/// <name> { ... } ... }` block. Each named entry is a `Property::Block`
/// whose inner properties follow this spec.
const TABLE_ENTRY_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "load",
        required: false,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "max",
        required: false,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "ttl",
        required: false,
        kind: PropertyValueKind::Int,
    },
];

pub(super) fn analyze_all(config: &CompiledConfig, diags: &mut Vec<Diagnostic>) {
    if let Some(props) = config.global_blocks.get("control") {
        emit_findings("control", &ds::validate(props, CONTROL_SCHEMA), diags);
    }
    if let Some(props) = config.global_blocks.get("geoip") {
        emit_findings("geoip", &ds::validate(props, GEOIP_SCHEMA), diags);
    }
    if let Some(props) = config.global_blocks.get("table") {
        analyze_table(props, diags);
    }
}

fn analyze_table(props: &[Property], diags: &mut Vec<Diagnostic>) {
    for entry in props {
        match entry {
            Property::Block {
                key: table_name,
                properties: inner,
                ..
            } => {
                let surface = format!("table '{}'", table_name);
                emit_findings(&surface, &ds::validate(inner, TABLE_ENTRY_SCHEMA), diags);
            }
            Property::KeyValue {
                key,
                key_span,
                value_span,
                ..
            } => {
                // `table { foo "bar" }` — the user wrote a scalar where a
                // named sub-block was expected. The schema doesn't model
                // "block-of-blocks only" directly, so we emit the
                // diagnostic by hand.
                let mut d = Diagnostic::error_kind(
                    DiagKind::PropertySchema,
                    format!(
                        "table '{}': expects a block ({{ ... }}), not a scalar value",
                        key
                    ),
                )
                .with_span(key_span.or(*value_span));
                d.help = None;
                diags.push(d);
            }
        }
    }
}

fn emit_findings(surface: &str, errs: &[ds::SchemaError], diags: &mut Vec<Diagnostic>) {
    for err in errs {
        let mut d = Diagnostic::error_kind(
            DiagKind::PropertySchema,
            format!("{}: {}", surface, err),
        )
        .with_span(err.primary_span());
        if let Some(s) = &err.did_you_mean {
            d = d.with_help(format!("did you mean `{}`?", s));
        }
        diags.push(d);
    }
}
