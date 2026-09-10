//! `balerix plugin package|install|remove` without a daemon, through the
//! binary, under a private HOME.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\n";
const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"true\"\n";

fn balerix(home: &Path) -> Command {
    let mut c = Command::cargo_bin("balerix").unwrap();
    c.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL")
        .current_dir(home);
    c
}

#[test]
fn package_install_and_remove_edit_plugins_yaml_offline() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let pkg = dir.path().join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(pkg.join("mise.toml"), MISE).unwrap();

    let out = balerix(&home)
        .args(["plugin", "package", &pkg.display().to_string()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("hello-0.1.0.tar.gz\nsha256: "), "{out}");
    let digest = out.trim().rsplit(' ').next().unwrap().to_string();
    assert_eq!(digest.len(), 64);
    let tarball = home.join("hello-0.1.0.tar.gz");
    assert!(tarball.exists());

    balerix(&home)
        .args([
            "plugin",
            "install",
            &tarball.display().to_string(),
            "--sha256",
            &"0".repeat(64),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("digest mismatch"));

    balerix(&home)
        .args(["plugin", "install", &tarball.display().to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("declared hello 0.1.0"))
        .stdout(predicate::str::contains(&digest))
        .stdout(predicate::str::contains("daemon not running"));
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(yaml.contains("name: hello"), "{yaml}");
    assert!(yaml.contains(&tarball.display().to_string()), "{yaml}");
    assert!(yaml.contains(&format!("sha256: {digest}")), "{yaml}");

    balerix(&home)
        .args(["plugin", "install", &pkg.display().to_string()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already declared"));

    balerix(&home)
        .args(["plugin", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));

    balerix(&home)
        .args(["plugin", "remove", "hello", "--purge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--purge needs a running daemon"));
    // the entry was removed even though the purge could not run
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(!yaml.contains("name: hello"), "{yaml}");

    balerix(&home)
        .args(["plugin", "remove", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not declared"));

    // a URL source is declarable in the format but not installable: this
    // build has no TLS, so `plugin install` says so instead of fetching
    balerix(&home)
        .args([
            "plugin",
            "install",
            "https://x/hello-0.1.0.tar.gz",
            "--sha256",
            &digest,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "URL sources need a TLS-enabled build; use `plugin package` and a tarball path",
        ));

    // a directory source records the absolute path and no digest
    balerix(&home)
        .args(["plugin", "install", &pkg.display().to_string()])
        .assert()
        .success();
    let yaml = fs::read_to_string(home.join(".config/balerix/plugins.yaml")).unwrap();
    assert!(
        yaml.contains(&fs::canonicalize(&pkg).unwrap().display().to_string()),
        "{yaml}"
    );
    assert!(!yaml.contains("sha256"), "{yaml}");
}
