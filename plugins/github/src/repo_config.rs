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

/// What `prepare` makes of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    /// The `PUT plugin-host/fleets/<name>` body.
    pub file: Value,
    /// The file's one crew key, which the plugin's agents run under
    /// (`<fleet>/<crew>/issue-<n>`, #258); `repo` by convention.
    pub crew: String,
}

/// §6: check, then set `name`, default the crew's `ref`, add one agent
/// per live session.
pub fn prepare(
    text: &str,
    repo: &str,
    fleet: &str,
    default_branch: &str,
    live: &[Live],
) -> Result<Prepared, String> {
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
    // The key names the agents (`<fleet>/<crew>/issue-<n>`, #258) and is
    // posted in refusals, so it is checked before it is echoed anywhere.
    let Some((crew_name, crew)) = crews.iter_mut().next() else {
        unreachable!("len checked")
    };
    if let Err(reason) = check_name(crew_name) {
        return Err(format!(
            ".balerix.yaml: crews: the crew key is not a valid name ({reason}); \
             use 1 to 63 of a-z, 0-9 and '-'"
        ));
    }
    let crew_name = crew_name.clone();
    let Value::Object(crew) = crew else {
        return Err(format!(
            ".balerix.yaml: crews.{crew_name}: expected a mapping"
        ));
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
            ".balerix.yaml: crews.{crew_name}.agents.{}: issue-<n> and pr-<n> are the plugin's names",
            shown(taken)
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
    Ok(Prepared {
        file,
        crew: crew_name,
    })
}

/// The daemon's shared name rule (`balerix_core::name::validate_name`,
/// which a plugin cannot depend on): `[a-z0-9-]`, 1 to 63, no `-` at
/// either end.
pub fn check_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("empty");
    }
    if s.len() > 63 {
        return Err("longer than 63 characters");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("characters other than a-z, 0-9 and '-'");
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err("starts or ends with '-'");
    }
    Ok(())
}

/// A key from the file as a public comment may show it: at most 63
/// characters.
fn shown(key: &str) -> String {
    match key.char_indices().nth(63) {
        Some((i, _)) => format!("{}…", &key[..i]),
        None => key.to_string(),
    }
}

/// Moves the file's one crew from `from` to `to`, for a crew key
/// renamed while sessions run under the old one (#258).
pub fn rename_crew(file: &mut Value, from: &str, to: &str) {
    if let Some(crews) = file.get_mut("crews").and_then(Value::as_object_mut)
        && let Some(crew) = crews.remove(from)
    {
        crews.insert(to.to_string(), crew);
    }
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
        assert_eq!(v.crew, "repo");
        let v = v.file;
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
        let mut after = prepare(FILE, "acme/payments", "gh-acme-payments", "main", &[])
            .unwrap()
            .file;
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
        let v = prepare(&f, "acme/payments", "f", "main", &[]).unwrap().file;
        assert_eq!(v["crews"]["repo"]["ref"], "release");
    }

    /// #258: any crew key is the crew the agents run under.
    #[test]
    fn the_crew_key_is_returned_and_carries_the_agents() {
        let f = FILE.replace("crews:\n  repo:\n", "crews:\n  backend:\n");
        let p = prepare(&f, "acme/payments", "f", "main", &[issue(12)]).unwrap();
        assert_eq!(p.crew, "backend");
        assert_eq!(
            p.file["crews"]["backend"]["agents"]["issue-12"]["plugins"]["github"]["number"],
            12
        );
        assert_eq!(p.file["crews"]["backend"]["ref"], "main");
        let mut file = p.file;
        rename_crew(&mut file, "backend", "repo");
        assert!(file["crews"].get("backend").is_none());
        assert_eq!(
            file["crews"]["repo"]["agents"]["issue-12"]["plugins"]["github"]["number"],
            12
        );
    }

    /// #258: the key becomes part of agent ids, and a refusal is posted
    /// publicly: it is checked by the name rule and never echoed.
    #[test]
    fn a_crew_key_that_is_not_a_name_is_refused_without_echoing_it() {
        let long = "x".repeat(1000);
        for key in ["Backend", "my_crew", "-a", "\"a/b\"", long.as_str()] {
            let f = FILE.replace("crews:\n  repo:\n", &format!("crews:\n  {key}:\n"));
            let e = prepare(&f, "acme/payments", "f", "main", &[]).unwrap_err();
            assert!(
                e.starts_with(".balerix.yaml: crews: the crew key is not a valid name ("),
                "{}: {e}",
                key.len()
            );
            assert!(e.len() < 200, "{e}");
        }
        let f = FILE.replace("reviewer:", &format!("issue-{}:", "9".repeat(1000)));
        let e = prepare(&f, "acme/payments", "f", "main", &[]).unwrap_err();
        assert!(e.len() < 200 && e.contains("…"), "{e}");
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
