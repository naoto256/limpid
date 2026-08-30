//! `csv_parse(text, field_names)` — parse a single CSV row into a named
//! object.
//!
//! Motivation: vendors like Palo Alto Networks emit syslog payloads as
//! a long flat CSV with positional columns. A proper CSV reader is the
//! only robust way; this primitive stays minimal.
//!
//! * Exactly one row parsed.
//! * `field_names` is an array of strings.
//! * Each field name maps to its column position. Empty field names
//!   silently skip that column.
//! * Empty cells become `null`.
//! * Extra columns dropped; missing columns produce `null`.
//!
//! CSV dialect: RFC 4180-ish — comma separator, double-quote quoting,
//! doubled-quote for embedded quote.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};

use crate::dsl::field_schema::FieldType;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "csv_parse",
        FunctionSig::fixed(
            &[FieldType::String, FieldType::Any],
            FieldType::union(FieldType::Object, FieldType::Null),
        ),
        |arena, args, _event| Ok(parse(arena, &args[0], &args[1])),
    );
}

fn parse<'bump>(
    arena: &'bump EventArena<'bump>,
    text: &Value<'bump>,
    field_names: &Value<'bump>,
) -> Value<'bump> {
    let text = match text {
        Value::String(s) => *s,
        _ => return Value::Null,
    };
    let names: Vec<&str> = match field_names {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => *s,
                _ => "", // non-string entries silently skip
            })
            .collect(),
        _ => return Value::Null,
    };
    if names.is_empty() {
        return Value::empty_object();
    }

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());

    let record = match rdr.records().next() {
        Some(Ok(r)) => r,
        _ => return Value::Null,
    };

    let mut builder = ObjectBuilder::with_capacity(arena, names.len());
    for (i, name) in names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let cell = record.get(i);
        let value = match cell {
            Some("") | None => Value::Null,
            Some(s) => Value::String(arena.alloc_str(s)),
        };
        builder.push_str(name, value);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::ArrayBuilder;

    fn names<'bump>(arena: &'bump EventArena<'bump>, values: &[Option<&str>]) -> Value<'bump> {
        let mut builder = ArrayBuilder::with_capacity(arena, values.len());
        for value in values {
            builder.push(match value {
                Some(value) => Value::String(arena.alloc_str(value)),
                None => Value::Int(7),
            });
        }
        builder.finish()
    }

    #[test]
    fn extra_cells_are_dropped_and_empty_names_keep_position() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let text = Value::String("one,ignored,two,extra");
        let fields = names(&arena, &[Some("first"), Some(""), Some("third")]);

        let Value::Object(entries) = parse(&arena, &text, &fields) else {
            panic!("csv_parse should return an object");
        };
        assert_eq!(
            entries,
            [
                ("first", Value::String("one")),
                ("third", Value::String("two")),
            ]
        );
    }

    #[test]
    fn missing_empty_and_non_string_named_cells_are_honest() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let text = Value::String("one,");
        let fields = names(
            &arena,
            &[Some("first"), Some("empty"), None, Some("missing")],
        );

        let Value::Object(entries) = parse(&arena, &text, &fields) else {
            panic!("csv_parse should return an object");
        };
        assert_eq!(
            entries,
            [
                ("first", Value::String("one")),
                ("empty", Value::Null),
                ("missing", Value::Null),
            ]
        );
    }

    #[test]
    fn duplicate_names_preserve_runtime_order_for_last_wins_merge() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let text = Value::String("first,second");
        let fields = names(&arena, &[Some("value"), Some("value")]);

        let Value::Object(entries) = parse(&arena, &text, &fields) else {
            panic!("csv_parse should return an object");
        };
        assert_eq!(
            entries,
            [
                ("value", Value::String("first")),
                ("value", Value::String("second")),
            ]
        );
    }
}
