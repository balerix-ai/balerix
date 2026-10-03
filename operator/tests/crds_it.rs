#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §4, §13, §14.1: five namespaced kinds, printed by `crds`, and
//! the committed files are what the types generate.
use std::process::Command;

use balerix_operator::api;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-operator");

#[test]
fn version_prints_the_core_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("balerix-operator {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn crds_prints_five_documents() {
    let out = Command::new(BIN).arg("crds").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let docs: Vec<serde_json::Value> = text
        .split("\n---\n")
        .map(|d| serde_norway::from_str(d).unwrap())
        .collect();
    let kinds: Vec<&str> = docs
        .iter()
        .map(|d| d["spec"]["names"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Daemon", "Fleet", "Crew", "Agent", "Plugin"]);
    for d in &docs {
        assert_eq!(d["kind"], "CustomResourceDefinition");
        assert_eq!(d["spec"]["group"], "balerix.ai");
        assert_eq!(d["spec"]["scope"], "Namespaced");
        let v = &d["spec"]["versions"][0];
        assert_eq!(v["name"], "v1alpha1");
        assert!(
            v["subresources"]["status"].is_object(),
            "a status subresource"
        );
        let status = &v["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
        assert!(status["conditions"].is_object());
        assert!(status["observedGeneration"].is_object());
    }
}

#[test]
fn the_settings_layers_are_open_objects_and_an_agent_prints_its_phase() {
    let fleet = serde_json::to_value(&api::crds()[1]).unwrap();
    let spec = &fleet["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
    assert_eq!(
        spec["defaults"]["x-kubernetes-preserve-unknown-fields"],
        true
    );
    let crew = &spec["crews"]["additionalProperties"]["properties"];
    assert_eq!(
        crew["defaults"]["x-kubernetes-preserve-unknown-fields"],
        true
    );
    assert_eq!(crew["agents"]["x-kubernetes-preserve-unknown-fields"], true);
    assert_eq!(
        spec["retain"]["enum"],
        serde_json::json!(["None", "Branches"])
    );
    let agent = serde_json::to_value(&api::crds()[3]).unwrap();
    let columns: Vec<&str> = agent["spec"]["versions"][0]["additionalPrinterColumns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["Phase", "Restarts", "Age"]);
}

/// `mise run crds` regenerates them; this is the check that fails when a
/// type changed and the files did not (§14.1).
#[test]
fn the_committed_files_are_what_the_types_generate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crds");
    let files = api::crd_files().unwrap();
    assert_eq!(files.len(), 5);
    for (name, yaml) in &files {
        let committed = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}; run `mise run crds`"));
        assert_eq!(&committed, yaml, "{name} is stale; run `mise run crds`");
    }
    let on_disk = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(on_disk, 5, "a file in operator/crds no type generates");
}
