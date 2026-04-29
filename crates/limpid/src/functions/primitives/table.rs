//! `table_lookup` / `table_upsert` / `table_delete` — key-value table
//! primitives. The three functions share a backing [`TableStore`]
//! (built from the `table { ... }` global blocks at startup).
//!
//! Storage holds `OwnedValue` because tables outlive any single per-event
//! arena. The DSL surface speaks `Value<'bump>`, so:
//!
//! - `table_lookup(...)` views the stored OwnedValue into the call's arena.
//! - `table_upsert(..., value)` converts the borrowed value to OwnedValue
//!   before inserting.

use crate::dsl::value::Value;

use super::val_to_str;
use crate::functions::table::TableStore;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry, table_store: TableStore) {
    {
        let store = table_store.clone();
        reg.register_with_sig(
            "table_lookup",
            FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Any),
            move |arena, args, _event| {
                let table_name = val_to_str(&args[0])?;
                let key = val_to_str(&args[1])?;
                Ok(store.lookup_view(arena, &table_name, &key))
            },
        );
    }

    {
        let store = table_store.clone();
        reg.register_with_sig(
            "table_upsert",
            FunctionSig::optional(
                &[
                    FieldType::String,
                    FieldType::String,
                    FieldType::Any,
                    FieldType::Int,
                ],
                3,
                FieldType::Null,
            ),
            move |_arena, args, _event| {
                let table_name = val_to_str(&args[0])?;
                let key = val_to_str(&args[1])?;
                let value = args[2].to_owned_value();
                if args.len() == 3 {
                    store.upsert_with_default(&table_name, &key, value);
                } else {
                    let secs = match &args[3] {
                        Value::Int(n) if *n >= 0 => Some(*n as u64),
                        Value::Float(f) if f.is_finite() && *f >= 0.0 => Some(*f as u64),
                        other => {
                            tracing::warn!(
                                "table_upsert: expire must be a number, got {} — using table default TTL",
                                other
                            );
                            None
                        }
                    };
                    match secs {
                        // 0 means "no expiry" — explicit caller intent.
                        Some(0) => store.upsert(&table_name, &key, value, None),
                        Some(s) => store.upsert(
                            &table_name,
                            &key,
                            value,
                            Some(std::time::Duration::from_secs(s)),
                        ),
                        None => store.upsert_with_default(&table_name, &key, value),
                    };
                }
                Ok(Value::Null)
            },
        );
    }

    {
        let store = table_store;
        reg.register_with_sig(
            "table_delete",
            FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Null),
            move |_arena, args, _event| {
                let table_name = val_to_str(&args[0])?;
                let key = val_to_str(&args[1])?;
                store.delete(&table_name, &key);
                Ok(Value::Null)
            },
        );
    }
}
