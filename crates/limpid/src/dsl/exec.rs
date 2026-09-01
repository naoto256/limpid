//! Assignment helpers shared by the sealed execution IR.

use anyhow::Result;
use bytes::Bytes;

use super::arena::EventArena;
use super::ast::AssignTarget;
use super::eval::value_to_string;
use super::value::{ObjectBuilder, Value};
use crate::event::BorrowedEvent;

pub(crate) fn apply_assign<'bump>(
    event: &mut BorrowedEvent<'bump>,
    target: &AssignTarget,
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) -> Result<()> {
    match target {
        AssignTarget::Egress => {
            // Egress outlives the per-event arena. Preserve arbitrary bytes;
            // a UTF-8 round trip would corrupt protobuf and raw payloads.
            event.egress = match value {
                Value::Bytes(bytes) => Bytes::copy_from_slice(bytes),
                Value::String(text) => Bytes::copy_from_slice(text.as_bytes()),
                other => Bytes::from(value_to_string(&other)),
            };
            Ok(())
        }
        AssignTarget::Workspace(path) => {
            set_workspace_path(event, path, value, arena);
            Ok(())
        }
    }
}

fn set_workspace_path<'bump>(
    event: &mut BorrowedEvent<'bump>,
    path: &[String],
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) {
    if path.len() == 1 {
        event.workspace_set_str(arena, &path[0], value);
        return;
    }

    let head = path[0].as_str();
    let updated = set_object_path(event.workspace_get(head), &path[1..], value, arena);
    event.workspace_set_str(arena, head, updated);
}

fn set_object_path<'bump>(
    current: Option<Value<'bump>>,
    path: &[String],
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) -> Value<'bump> {
    if path.is_empty() {
        return value;
    }

    let head = path[0].as_str();
    let existing_entries: &[(&str, Value<'bump>)] = match current {
        Some(Value::Object(entries)) => entries,
        _ => &[],
    };
    let mut builder = ObjectBuilder::with_capacity(arena, existing_entries.len() + 1);
    let mut placed = false;
    for (key, existing) in existing_entries {
        if *key == head {
            builder.push(
                key,
                set_object_path(Some(*existing), &path[1..], value, arena),
            );
            placed = true;
        } else {
            builder.push(key, *existing);
        }
    }
    if !placed {
        builder.push(
            arena.alloc_str(head),
            set_object_path(None, &path[1..], value, arena),
        );
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::dsl::value::OwnedValue;
    use crate::event::Event;

    #[test]
    fn assignment_preserves_binary_egress_and_nested_workspace() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let event = Event::new(Bytes::from_static(b"input"), "127.0.0.1:1".parse().unwrap());
        let mut borrowed = event.view_in(&arena);

        apply_assign(
            &mut borrowed,
            &AssignTarget::Egress,
            Value::Bytes(&[0, 0xff, 1]),
            &arena,
        )
        .unwrap();
        apply_assign(
            &mut borrowed,
            &AssignTarget::Workspace(vec!["nested".into(), "value".into()]),
            Value::String("ok"),
            &arena,
        )
        .unwrap();

        assert_eq!(borrowed.egress.as_ref(), &[0, 0xff, 1]);
        let owned = borrowed.to_owned();
        let Some(OwnedValue::Object(nested)) = owned.workspace.get("nested") else {
            panic!("nested workspace object missing");
        };
        assert_eq!(nested.get("value"), Some(&OwnedValue::String("ok".into())));
    }
}
