//! Layered merge of settings blocks (spec §5). Rules: maps deep-merge,
//! scalars overlay-wins, lists replace, an explicit `null` in the overlay
//! deletes the key. Output never contains a `null` map value.
//!
//! The operation is a left fold, not associative: `{x: null}` means "reset
//! x relative to everything below me", which only makes sense in order.

use serde_json::{Map, Value};

/// Merges `overlay` onto `base`.
pub fn merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = Map::new();
            for (k, bv) in b {
                if !o.contains_key(k) && !bv.is_null() {
                    out.insert(k.clone(), strip_nulls(bv));
                }
            }
            for (k, ov) in o {
                if ov.is_null() {
                    continue; // delete
                }
                let merged = match b.get(k) {
                    Some(bv) => merge(bv, ov),
                    None => strip_nulls(ov),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (_, Value::Null) => strip_nulls(base),
        (_, o) => strip_nulls(o),
    }
}

/// Folds `layers` left-to-right onto an empty map; later layers win.
pub fn merge_layers<'a>(layers: impl IntoIterator<Item = &'a Value>) -> Value {
    layers
        .into_iter()
        .fold(Value::Object(Map::new()), |acc, layer| merge(&acc, layer))
}

/// Removes `null` map entries at every depth. Array elements are kept as-is.
pub fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn scalar_in_overlay_wins() {
        assert_eq!(merge(&json!({"a": 1}), &json!({"a": 2})), json!({"a": 2}));
        assert_eq!(
            merge(&json!({"a": "x"}), &json!({"a": true})),
            json!({"a": true})
        );
    }

    #[test]
    fn maps_deep_merge() {
        let base = json!({"claude": {"settings": {"model": "sonnet", "theme": "dark"}}});
        let over = json!({"claude": {"settings": {"model": "opus"}, "resume": true}});
        assert_eq!(
            merge(&base, &over),
            json!({"claude": {"settings": {"model": "opus", "theme": "dark"}, "resume": true}})
        );
    }

    #[test]
    fn lists_replace_never_concatenate() {
        assert_eq!(
            merge(&json!({"args": ["--a", "--b"]}), &json!({"args": ["--c"]})),
            json!({"args": ["--c"]})
        );
        assert_eq!(
            merge(&json!({"args": ["--a"]}), &json!({"args": []})),
            json!({"args": []})
        );
    }

    #[test]
    fn null_in_overlay_deletes_the_key() {
        assert_eq!(
            merge(&json!({"a": 1, "b": 2}), &json!({"a": null})),
            json!({"b": 2})
        );
        assert_eq!(
            merge(
                &json!({"env": {"RUST_LOG": "info", "FOO": "1"}}),
                &json!({"env": {"FOO": null}})
            ),
            json!({"env": {"RUST_LOG": "info"}})
        );
    }

    #[test]
    fn deleting_a_map_then_adding_into_it_starts_fresh() {
        // fleet sets x.y, crew deletes x, agent sets x.z → only z survives
        let layers = [
            json!({"x": {"y": 1}}),
            json!({"x": null}),
            json!({"x": {"z": 2}}),
        ];
        assert_eq!(merge_layers(&layers), json!({"x": {"z": 2}}));
    }

    #[test]
    fn overlay_kind_change_replaces() {
        assert_eq!(
            merge(&json!({"a": {"b": 1}}), &json!({"a": 5})),
            json!({"a": 5})
        );
        assert_eq!(
            merge(&json!({"a": 5}), &json!({"a": {"b": 1}})),
            json!({"a": {"b": 1}})
        );
    }

    #[test]
    fn empty_overlay_returns_base_without_nulls() {
        assert_eq!(
            merge(&json!({"a": 1, "gone": null}), &json!({})),
            json!({"a": 1})
        );
    }

    #[test]
    fn merge_layers_folds_left_from_empty() {
        assert_eq!(merge_layers(std::iter::empty()), json!({}));
        assert_eq!(
            merge_layers(&[json!({"a": 1}), json!({"b": 2}), json!({"a": 3})]),
            json!({"a": 3, "b": 2})
        );
    }

    #[test]
    fn strip_nulls_removes_null_entries_at_every_depth() {
        assert_eq!(
            strip_nulls(&json!({"a": null, "b": {"c": null, "d": 1}, "e": [null]})),
            json!({"b": {"d": 1}, "e": [null]})
        );
    }

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(Value::from),
            "[a-z]{0,3}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
                prop::collection::btree_map("[a-c]", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// Objects only — the top-level settings layers are always mappings.
    fn arb_object() -> impl Strategy<Value = Value> {
        prop::collection::btree_map("[a-c]", arb_json(), 0..4)
            .prop_map(|m| Value::Object(m.into_iter().collect()))
    }

    fn has_null_map_value(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.values().any(|x| x.is_null() || has_null_map_value(x)),
            Value::Array(a) => a.iter().any(has_null_map_value),
            _ => false,
        }
    }

    proptest! {
        #[test]
        fn output_never_holds_null_map_values(a in arb_json(), b in arb_json()) {
            prop_assert!(!has_null_map_value(&merge(&a, &b)));
        }

        #[test]
        fn empty_overlay_is_strip_nulls(a in arb_object()) {
            // Only meaningful for mappings: `{}` onto a scalar is an overlay that wins.
            prop_assert_eq!(merge(&a, &json!({})), strip_nulls(&a));
        }

        #[test]
        fn merging_with_self_is_strip_nulls(a in arb_json()) {
            prop_assert_eq!(merge(&a, &a), strip_nulls(&a));
        }

        #[test]
        fn reapplying_the_overlay_changes_nothing(a in arb_json(), b in arb_json()) {
            let once = merge(&a, &b);
            prop_assert_eq!(merge(&once, &b), once);
        }
    }
}
