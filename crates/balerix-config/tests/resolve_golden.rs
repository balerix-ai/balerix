//! Golden snapshots of resolved specs. Review every `.snap.new` by hand
//! before `cargo insta accept` — the values to check are listed in the plan.

use balerix_config::{ResolveOptions, parse, resolve};
use serde_json::json;

fn read(path: &str) -> String {
    let full = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

#[test]
fn payments_example_resolves_to_known_spec() {
    let file = parse(&read("../../examples/payments.yaml")).unwrap();
    let host = json!({ "theme": "dark", "model": "haiku" });
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: None,
            host_claude_settings: Some(host),
        },
    )
    .unwrap();
    insta::assert_yaml_snapshot!("payments", spec);
}

#[test]
fn overrides_exercise_null_delete_and_list_replace() {
    let file = parse(&read("tests/fixtures/overrides.yaml")).unwrap();
    let spec = resolve(&file, &ResolveOptions::default()).unwrap();
    insta::assert_yaml_snapshot!("overrides", spec);
}
