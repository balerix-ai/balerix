#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.2 step 1 in a pod layout: the sidecar materialises an agent
//! over a crew cache it can only read, mounted objects-only.
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use balerix_api::{AgentSettings, CredentialBundle, GitAuth, GitSettings};
use balerix_core::{AgentId, HookTarget, MaterializeError, Materializer, RepoRef, ResolvedAgent};
use balerix_runtime::sandbox::{SelfTestError, sandbox_self_test};
use balerix_runtime::{Runtime, StateLayout};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// An origin with one commit, and a crew cache of it holding only
/// `.git/objects`, read-only, the way the pod mounts it.
fn origin_and_objects_only_cache(root: &Path, layout: &StateLayout, id: &AgentId) -> RepoRef {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("f"), "one\n").unwrap();
    git(&origin, &["add", "f"]);
    git(&origin, &["commit", "-q", "-m", "one"]);
    let full = root.join("cache-full");
    let out = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&origin)
        .arg(&full)
        .output()
        .unwrap();
    assert!(out.status.success());
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    std::fs::create_dir_all(objects.parent().unwrap()).unwrap();
    let cp = Command::new("cp")
        .arg("-r")
        .arg(full.join("objects"))
        .arg(&objects)
        .output()
        .unwrap();
    assert!(cp.status.success());
    chmod_tree(&objects, 0o555, 0o444);
    RepoRef::parse(&format!("file://{}", origin.display())).unwrap()
}

fn chmod_tree(dir: &Path, dirs: u32, files: u32) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            chmod_tree(&p, dirs, files);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(dirs)).unwrap();
        } else {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(files)).unwrap();
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(dirs)).unwrap();
}

fn mtimes(dir: &Path) -> BTreeMap<String, std::time::SystemTime> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, out: &mut BTreeMap<String, std::time::SystemTime>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            out.insert(
                p.display().to_string(),
                e.metadata().unwrap().modified().unwrap(),
            );
            if p.is_dir() {
                walk(&p, out);
            }
        }
    }
    walk(dir, &mut out);
    out
}

fn agent(id: &AgentId, repo: RepoRef) -> ResolvedAgent {
    ResolvedAgent {
        id: id.clone(),
        repo,
        git_ref: "main".into(),
        git: GitSettings {
            push: false,
            auth: GitAuth::None,
            ..GitSettings::default()
        },
        settings: AgentSettings::default(),
    }
}

#[test]
fn a_pod_materialize_clones_from_the_objects_only_cache_and_writes_nothing_into_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-pod");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let repo = origin_and_objects_only_cache(&root, &layout, &id);
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    let before = mtimes(&objects);
    let rt = Runtime::new(layout.clone(), tools.clone());
    let creds = CredentialBundle::default();
    let fleet_tools = BTreeMap::new();
    let crew_tools = BTreeMap::new();
    rt.ensure_crew(
        &id.crew_ref(),
        &repo,
        "main",
        &GitSettings {
            push: false,
            auth: GitAuth::None,
            ..GitSettings::default()
        },
        &creds,
        balerix_core::CrewTools {
            fleet: &fleet_tools,
            crew: &crew_tools,
        },
    )
    .unwrap();
    assert!(
        !layout.crew(&id.crew_ref()).mise_pool().exists(),
        "a pod ensure_crew installs no pool: the Job did"
    );
    let plan = rt
        .materialize(
            &agent(&id, repo),
            &creds,
            &HookTarget {
                url: "http://127.0.0.1:7643".into(),
                secret: "s".into(),
            },
        )
        .unwrap();
    let a = layout.agent(&id);
    assert_eq!(plan.script, a.launch);
    assert!(a.workspace.join(".git").is_dir());
    let alternates =
        std::fs::read_to_string(a.workspace.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(alternates.trim(), objects.display().to_string());
    assert_eq!(mtimes(&objects), before, "the cache was written");
    assert!(a.installed_marker().is_file());
    assert!(
        layout.crew(&id.crew_ref()).logs.join("git.log").is_file(),
        "the clone's git calls are logged on the claim"
    );
    assert!(
        !root.join("shared/crew/logs").exists(),
        "nothing is written under the crew root, a read-only mount in a pod"
    );
    chmod_tree(&objects, 0o755, 0o644); // let TempRoot remove it
}

#[test]
fn a_pod_ensure_crew_without_a_synced_cache_names_the_job() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-pod-nocache");
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let rt = Runtime::new(layout.clone(), tools);
    let empty = BTreeMap::new();
    let err = rt
        .ensure_crew(
            &id.crew_ref(),
            &RepoRef::parse("acme/api").unwrap(),
            "main",
            &GitSettings {
                auth: GitAuth::None,
                ..GitSettings::default()
            },
            &CredentialBundle::default(),
            balerix_core::CrewTools {
                fleet: &empty,
                crew: &empty,
            },
        )
        .unwrap_err();
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    assert_eq!(
        err,
        MaterializeError::Invalid {
            id: "f/c".into(),
            message: format!(
                "crew cache not synced: {} is missing (the crew sync Job has not run)",
                objects.display()
            ),
        }
    );
}

#[test]
fn the_sandbox_self_test_passes_where_landlock_works_and_names_an_unavailable_sandbox() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("selftest");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let repo = origin_and_objects_only_cache(&root, &layout, &id);
    let rt = Runtime::new(layout.clone(), tools.clone());
    let out = rt
        .render_agent(
            &agent(&id, repo),
            &CredentialBundle::default(),
            &HookTarget {
                url: "http://127.0.0.1:7643".into(),
                secret: "s".into(),
            },
            &balerix_runtime::RenderOptions::default(),
        )
        .unwrap();
    assert!(out.plan.script.is_file());
    let paths = layout.agent(&id);
    sandbox_self_test(&tools, &paths).unwrap();

    // A nono that cannot set the sandbox up: the message nono gave at the
    // spike (§19.2), exit 1. The sidecar's termination message starts with
    // `SandboxUnavailable:`.
    let fake = root.join("fake-nono");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled.' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut broken = tools.clone();
    broken.nono = fake;
    assert_eq!(
        sandbox_self_test(&broken, &paths),
        Err(SelfTestError::Unavailable(
            "nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled.".into()
        ))
    );
    assert_eq!(
        sandbox_self_test(&broken, &paths).unwrap_err().to_string(),
        "SandboxUnavailable: nono: Sandbox initialization failed: Landlock not available. Requires Linux kernel 5.13+ with Landlock enabled."
    );
    let other = root.join("other-nono");
    std::fs::write(&other, "#!/bin/sh\necho 'nono: profile: bad' >&2\nexit 2\n").unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    broken.nono = other;
    assert_eq!(
        sandbox_self_test(&broken, &paths),
        Err(SelfTestError::Failed("nono: profile: bad".into()))
    );
    let objects = layout.crew(&id.crew_ref()).cache_objects();
    chmod_tree(&objects, 0o755, 0o644);
}
#[test]
fn a_pod_ensure_crew_writes_the_gh_config_before_it_checks_the_cache() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-pod-gh");
    let id: AgentId = "f/c/a".parse().unwrap();
    let layout = support::pod_layout(&root, &id);
    let rt = Runtime::new(layout.clone(), tools);
    let empty = BTreeMap::new();
    let creds = CredentialBundle {
        gh_token: Some("gho_T".into()),
        ..CredentialBundle::default()
    };
    // no synced cache: the call fails, but the gh config a clone needs is on the claim
    rt.ensure_crew(
        &id.crew_ref(),
        &RepoRef::parse("acme/api").unwrap(),
        "main",
        &GitSettings {
            auth: GitAuth::Gh,
            ..GitSettings::default()
        },
        &creds,
        balerix_core::CrewTools {
            fleet: &empty,
            crew: &empty,
        },
    )
    .unwrap_err();
    let hosts = layout.fleet_gh_dir(&id.crew_ref().fleet).join("hosts.yml");
    assert!(std::fs::read_to_string(hosts).unwrap().contains("gho_T"));
}
