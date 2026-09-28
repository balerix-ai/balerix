//! The repository's fleet file (Spec M §6): an ordinary fleet file with
//! one crew naming this repository, read from the default branch, into
//! which the plugin injects its agents. Every refusal is the message
//! posted on the issue.

use serde_json::{Map, Value, json};

use crate::config::Kind;
use crate::mention::is_plugin_agent;

/// Larger files are refused before parsing.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// A session the fleet must carry an agent for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub kind: Kind,
    pub number: u64,
    /// The PR's head ref (`branch`, Spec L §6); `None` for an issue.
    pub head: Option<String>,
}

/// §6: check, then set `name`, default the crew's `ref`, add one agent
/// per live session. The result is the `PUT plugin-host/fleets/<name>`
/// body.
pub fn prepare(
    text: &str,
    repo: &str,
    fleet: &str,
    default_branch: &str,
    live: &[Live],
) -> Result<Value, String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(format!(".balerix.yaml: larger than {MAX_FILE_BYTES} bytes"));
    }
    let mut file: Value =
        serde_norway::from_str(text).map_err(|e| format!(".balerix.yaml: invalid YAML: {e}"))?;
    let Some(root) = file.as_object_mut() else {
        return Err(".balerix.yaml: expected a mapping".into());
    };
    if root.get("apiVersion").and_then(Value::as_str) != Some("balerix/v1") {
        return Err(".balerix.yaml: apiVersion: expected \"balerix/v1\"".into());
    }
    if root.get("kind").and_then(Value::as_str) != Some("Fleet") {
        return Err(".balerix.yaml: kind: expected \"Fleet\"".into());
    }
    let crews = match root.get_mut("crews") {
        Some(Value::Object(c)) => c,
        _ => return Err(".balerix.yaml: crews: expected a mapping with one entry".into()),
    };
    if crews.len() != 1 {
        return Err(format!(
            ".balerix.yaml: crews: expected exactly one crew, got {}",
            crews.len()
        ));
    }
    let (crew_name, crew) = match crews.iter_mut().next() {
        Some((k, Value::Object(c))) => (k.clone(), c),
        Some((k, _)) => return Err(format!(".balerix.yaml: crews.{k}: expected a mapping")),
        None => unreachable!("len checked"),
    };
    let declared = crew.get("repo").and_then(Value::as_str).unwrap_or("");
    if !names_repo(declared, repo) {
        return Err(format!(
            ".balerix.yaml: crews.{crew_name}.repo: expected {repo} (this repository), got {declared:?}"
        ));
    }
    if !crew.contains_key("ref") {
        crew.insert("ref".into(), json!(default_branch));
    }
    let agents = match crew.entry("agents").or_insert_with(|| json!({})) {
        Value::Object(a) => a,
        _ => {
            return Err(format!(
                ".balerix.yaml: crews.{crew_name}.agents: expected a mapping"
            ));
        }
    };
    if let Some(taken) = agents.keys().find(|k| is_plugin_agent(k)) {
        return Err(format!(
            ".balerix.yaml: crews.{crew_name}.agents.{taken}: issue-<n> and pr-<n> are the plugin's names"
        ));
    }
    for s in live {
        let mut agent = Map::new();
        agent.insert(
            "plugins".into(),
            json!({ "github": { "kind": s.kind, "number": s.number } }),
        );
        if let Some(head) = &s.head {
            agent.insert("branch".into(), json!(head));
        }
        agents.insert(s.kind.agent_name(s.number), Value::Object(agent));
    }
    root.insert("name".into(), json!(fleet));
    Ok(file)
}

/// `owner/name` case-insensitively, or a clone URL of it.
fn names_repo(declared: &str, repo: &str) -> bool {
    let d = declared.trim().trim_end_matches('/');
    let d = d.strip_suffix(".git").unwrap_or(d);
    let tail = d
        .rsplit_once(':')
        .map(|(_, t)| t)
        .filter(|_| d.starts_with("git@"))
        .unwrap_or_else(|| d.strip_prefix("https://github.com/").unwrap_or(d));
    tail.eq_ignore_ascii_case(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "apiVersion: balerix/v1\nkind: Fleet\ndefaults:\n  tools: { node: \"22.11.0\" }\ncrews:\n  repo:\n    repo: Acme/Payments\n    agents:\n      reviewer: { claude: { settings: { model: opus } } }\n";

    fn issue(n: u64) -> Live {
        Live {
            kind: Kind::Issue,
            number: n,
            head: None,
        }
    }

    #[test]
    fn a_good_file_gains_the_name_the_ref_and_the_agents() {
        let v = prepare(
            FILE,
            "acme/payments",
            "gh-acme-payments",
            "develop",
            &[
                issue(12),
                Live {
                    kind: Kind::Pr,
                    number: 34,
                    head: Some("feature/x".into()),
                },
            ],
        )
        .unwrap();
        insta::assert_json_snapshot!(v);
        assert_eq!(v["name"], "gh-acme-payments");
        assert_eq!(v["crews"]["repo"]["ref"], "develop");
        assert_eq!(v["crews"]["repo"]["agents"]["pr-34"]["branch"], "feature/x");
        assert_eq!(
            v["crews"]["repo"]["agents"]["issue-12"]["plugins"]["github"]["number"],
            12
        );
        assert_eq!(
            v["crews"]["repo"]["agents"]["reviewer"]["claude"]["settings"]["model"],
            "opus"
        );
    }

    #[test]
    fn injection_leaves_the_rest_of_the_file_intact() {
        let before: Value = serde_norway::from_str(FILE).unwrap();
        let mut after = prepare(FILE, "acme/payments", "gh-acme-payments", "main", &[]).unwrap();
        after.as_object_mut().unwrap().remove("name");
        after["crews"]["repo"]
            .as_object_mut()
            .unwrap()
            .remove("ref");
        assert_eq!(after, before);
    }

    #[test]
    fn an_explicit_ref_is_kept() {
        let f = FILE.replace(
            "    repo: Acme/Payments\n",
            "    repo: acme/payments\n    ref: release\n",
        );
        let v = prepare(&f, "acme/payments", "f", "main", &[]).unwrap();
        assert_eq!(v["crews"]["repo"]["ref"], "release");
    }

    #[test]
    fn every_refusal_names_what_is_wrong() {
        let cases: [(&str, &str); 7] = [
            (
                "apiVersion: balerix/v2\nkind: Fleet\ncrews: {}\n",
                ".balerix.yaml: apiVersion: expected \"balerix/v1\"",
            ),
            (
                "apiVersion: balerix/v1\nkind: Crew\ncrews: {}\n",
                ".balerix.yaml: kind: expected \"Fleet\"",
            ),
            (
                "apiVersion: balerix/v1\nkind: Fleet\n",
                ".balerix.yaml: crews: expected a mapping with one entry",
            ),
            (
                "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  a: { repo: acme/payments }\n  b: { repo: acme/payments }\n",
                ".balerix.yaml: crews: expected exactly one crew, got 2",
            ),
            (
                "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo: { repo: other/thing }\n",
                ".balerix.yaml: crews.repo.repo: expected acme/payments (this repository), got \"other/thing\"",
            ),
            (
                "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/payments\n    agents: { issue-7: {} }\n",
                ".balerix.yaml: crews.repo.agents.issue-7: issue-<n> and pr-<n> are the plugin's names",
            ),
            ("not: [valid", ".balerix.yaml: invalid YAML: "),
        ];
        for (text, want) in cases {
            let e = prepare(text, "acme/payments", "f", "main", &[]).unwrap_err();
            assert!(e.starts_with(want), "{text:?}: {e}");
        }
        let big = format!(
            "apiVersion: balerix/v1\nkind: Fleet\n# {}\n",
            "x".repeat(MAX_FILE_BYTES)
        );
        assert!(
            prepare(&big, "acme/payments", "f", "main", &[])
                .unwrap_err()
                .contains("larger than")
        );
    }

    #[test]
    fn the_repo_may_be_a_clone_url_or_differ_in_case() {
        for declared in [
            "acme/payments",
            "ACME/payments",
            "https://github.com/acme/payments.git",
            "git@github.com:acme/payments.git",
        ] {
            let f = FILE.replace("Acme/Payments", declared);
            assert!(
                prepare(&f, "acme/payments", "f", "main", &[]).is_ok(),
                "{declared}"
            );
        }
        assert!(!names_repo("acme/payments-v2", "acme/payments"));
    }
}
