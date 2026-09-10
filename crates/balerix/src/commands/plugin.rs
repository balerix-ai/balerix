//! `balerix plugin …` (plugins spec §10): edits `plugins.yaml`, syncs a
//! running daemon, lists, purges, packages.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{PluginEntry, PluginManifest, PluginStatus, PluginsFile, SyncReport};
use balerix_runtime::fsutil::write_atomic;
use balerix_server::plugins::config::NO_TLS;
use balerix_server::plugins::package::{create, sha256_hex, unpack};
use balerix_server::plugins::read_manifest;

use crate::cli::{
    ApiOnlyArgs, ListArgs, PluginInstallArgs, PluginOpenArgs, PluginPackageArgs, PluginRemoveArgs,
};
use crate::client::{Client, NOT_RUNNING};
use crate::wiring::layout_from_env;

const NOT_SYNCED: &str = "daemon not running; `balerix serve` syncs plugins at start\n";

fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn render_plugins(rows: &[PluginStatus]) -> String {
    if rows.is_empty() {
        return "no plugins\n".to_string();
    }
    let rows: Vec<[String; 7]> = rows
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.version.clone(),
                label(p.phase),
                p.listen.clone().unwrap_or_else(|| "-".to_string()),
                if p.routes { "yes" } else { "no" }.to_string(),
                p.active_agents.to_string(),
                p.message.clone(),
            ]
        })
        .collect();
    super::fleet::table(
        &[
            "NAME", "VERSION", "PHASE", "LISTEN", "ROUTES", "ACTIVE", "MESSAGE",
        ],
        &rows,
    )
}

pub fn render_sync(r: &SyncReport) -> String {
    let mut out = String::new();
    for (label, names) in [
        ("installed", &r.installed),
        ("stopped", &r.stopped),
        ("unchanged", &r.unchanged),
    ] {
        if !names.is_empty() {
            out.push_str(&format!("{label}: {}\n", names.join(", ")));
        }
    }
    if out.is_empty() {
        out.push_str("nothing to do\n");
    }
    out
}

pub fn plugins_file_path() -> Result<PathBuf> {
    Ok(layout_from_env()?.config_root.join("plugins.yaml"))
}

/// Missing file → empty. Not validated here: the daemon validates on sync.
pub fn load_file(path: &Path) -> Result<PluginsFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_norway::from_str(&text)
            .map_err(|e| anyhow!("{}: invalid plugins file: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PluginsFile::default()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Rewrites the file; comments are not preserved. Atomic (sibling temp file
/// then rename), so a crash or a full disk cannot leave the operator's
/// source of truth truncated — `serve` fail-fasts on an unreadable one.
///
/// 0600: an entry's `config` may hold a credential inline (spec G §4.1
/// permits an inline `password`; the `secrets` map is the alternative, not
/// the only route), and `write_atomic` sets the mode on the temp file it
/// renames into place, so any weaker mode here would silently widen a file
/// an operator had locked down on the next `plugin install`/`plugin
/// remove`. Nothing but the daemon, running as this same user, ever reads
/// it — the same rule `resolve_secrets` already enforces on a secret file.
pub fn save_file(path: &Path, file: &PluginsFile) -> Result<()> {
    let text = serde_norway::to_string(file)?;
    write_atomic(path, text.as_bytes(), 0o600).with_context(|| path.display().to_string())
}

pub fn install_entry(file: &mut PluginsFile, entry: PluginEntry) -> Result<()> {
    if file.plugins.iter().any(|p| p.name == entry.name) {
        bail!(
            "plugin {:?} is already declared in plugins.yaml",
            entry.name
        );
    }
    file.plugins.push(entry);
    Ok(())
}

pub fn remove_entry(file: &mut PluginsFile, name: &str) -> bool {
    let before = file.plugins.len();
    file.plugins.retain(|p| p.name != name);
    file.plugins.len() != before
}

/// A `remove` that changed nothing is an error, unless `--purge` was asked
/// for: the state and packages the entry left behind are still there to
/// delete, so the daemon call goes ahead.
fn should_bail(removed: bool, purge: bool) -> bool {
    !removed && !purge
}

/// Reads the manifest behind a source and decides what to record: the
/// absolute path or URL, and the digest for tarballs and URLs.
fn describe_source(
    source: &str,
    sha256: Option<&str>,
) -> Result<(String, Option<String>, PluginManifest)> {
    if source.starts_with("https://") {
        // enabled when a TLS provider is added; `package::fetch` and the
        // `Source::Url` plumbing behind it stay in place for that day
        bail!(NO_TLS);
    }
    if source.contains("://") {
        bail!("only https:// URLs, tarball paths and directories are accepted");
    }
    let path = std::fs::canonicalize(source).with_context(|| format!("{source}: not found"))?;
    if path.is_dir() {
        let manifest = read_manifest(&path)?;
        return Ok((path.display().to_string(), None, manifest));
    }
    let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
    let digest = sha256_hex(&bytes);
    if let Some(expected) = sha256
        && expected != digest
    {
        bail!(
            "digest mismatch for {} (expected {expected}, got {digest})",
            path.display()
        );
    }
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("pkg");
    unpack(&bytes, &dir)?;
    Ok((
        path.display().to_string(),
        Some(digest),
        read_manifest(&dir)?,
    ))
}

/// Syncs when a daemon is reachable; otherwise says `serve` will.
fn sync_if_running(api_url: Option<&str>) -> Result<String> {
    match Client::try_connect(api_url)? {
        Some(c) => match c.sync_plugins() {
            Ok(r) => Ok(render_sync(&r)),
            Err(e) if e.to_string().starts_with(NOT_RUNNING) => Ok(NOT_SYNCED.to_string()),
            Err(e) => Err(e),
        },
        None => Ok(NOT_SYNCED.to_string()),
    }
}

pub fn install_command(args: &PluginInstallArgs) -> Result<String> {
    let (source, sha256, manifest) = describe_source(&args.source, args.sha256.as_deref())?;
    let path = plugins_file_path()?;
    let mut file = load_file(&path)?;
    install_entry(
        &mut file,
        PluginEntry {
            name: manifest.name.clone(),
            source,
            sha256: sha256.clone(),
            secrets: Default::default(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        },
    )?;
    save_file(&path, &file)?;
    let mut out = format!(
        "declared {} {} in {}\n",
        manifest.name,
        manifest.version,
        path.display()
    );
    if let Some(d) = sha256 {
        out.push_str(&format!("sha256: {d}\n"));
    }
    out.push_str(&sync_if_running(args.api_url.as_deref())?);
    Ok(out)
}

pub fn sync_command(args: &ApiOnlyArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    Ok(render_sync(&client.sync_plugins()?))
}

pub fn list_command(args: &ListArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let rows = client.plugins()?;
    Ok(if args.json {
        serde_json::to_string_pretty(&rows)? + "\n"
    } else {
        render_plugins(&rows)
    })
}

pub fn remove_command(args: &PluginRemoveArgs) -> Result<String> {
    let path = plugins_file_path()?;
    let mut file = load_file(&path)?;
    if should_bail(remove_entry(&mut file, &args.name), args.purge) {
        bail!(
            "plugin {:?} is not declared in {}",
            args.name,
            path.display()
        );
    }
    save_file(&path, &file)?;
    if args.purge {
        let client = Client::connect(args.api_url.as_deref())
            .map_err(|e| anyhow!("{e}; --purge needs a running daemon (the entry was removed)"))?;
        let report = client.sync_plugins()?;
        client.purge_plugin(&args.name)?;
        return Ok(format!(
            "{}removed {} and purged its state\n",
            render_sync(&report),
            args.name
        ));
    }
    Ok(format!(
        "removed {} from plugins.yaml (state kept; --purge deletes it)\n{}",
        args.name,
        sync_if_running(args.api_url.as_deref())?
    ))
}

pub fn package_command(args: &PluginPackageArgs) -> Result<String> {
    let manifest = read_manifest(&args.dir)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}.tar.gz", manifest.name, manifest.version)));
    let digest = create(&args.dir, &out)?;
    Ok(format!("{}\nsha256: {digest}\n", out.display()))
}

/// `balerix plugin open <name>` (plugins spec §18.2): prints the login URL;
/// opening it in a browser sets the session cookie and lands on the
/// plugin's mount. Nothing is launched.
pub fn open_command(args: &PluginOpenArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    open_with(&client, &args.name)
}

/// The command against a connected client: the name is validated here,
/// the mount root gains its slash (a bare mount is the admin purge route).
fn open_with(client: &Client, name: &str) -> Result<String> {
    let name: balerix_core::AgentName = name
        .parse()
        .map_err(|e: balerix_core::NameError| anyhow!("plugin name: {e}"))?;
    Ok(format!(
        "{}\n",
        client.create_session(&format!("/v1/plugins/{name}/"))?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn open_asks_for_a_session_on_the_mount_root_and_prints_the_url() {
        let (url, rx) = crate::testutil::stub_server(
            "200 OK",
            r#"{"login_url":"http://127.0.0.1:7643/v1/login/abc123?to=/v1/plugins/web/"}"#,
        );
        let client = Client::new(url, "t".into());
        assert_eq!(
            open_with(&client, "web").unwrap(),
            "http://127.0.0.1:7643/v1/login/abc123?to=/v1/plugins/web/\n"
        );
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /v1/sessions HTTP/1.1"), "{raw}");
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "to": "/v1/plugins/web/" }));
        // a bad name never reaches the daemon
        let e = open_with(
            &Client::new("http://127.0.0.1:1".into(), "t".into()),
            "Nope",
        )
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("plugin name: "), "{e}");
    }

    #[test]
    fn renders_the_plugin_table_and_sync_report() {
        let rows = vec![
            PluginStatus {
                name: "flow".into(),
                version: "0.1.0".into(),
                phase: AgentPhase::Ready,
                listen: Some("127.0.0.1:4000".into()),
                routes: false,
                message: String::new(),
                active_agents: 2,
            },
            PluginStatus {
                name: "web".into(),
                version: "0.2.0".into(),
                phase: AgentPhase::Starting,
                listen: None,
                routes: true,
                message: "exited with status 1".into(),
                active_agents: 0,
            },
        ];
        assert_eq!(
            render_plugins(&rows),
            "NAME  VERSION  PHASE     LISTEN          ROUTES  ACTIVE  MESSAGE\n\
             flow  0.1.0    ready     127.0.0.1:4000  no      2\n\
             web   0.2.0    starting  -               yes     0       exited with status 1\n"
        );
        assert_eq!(render_plugins(&[]), "no plugins\n");
        let r = SyncReport {
            installed: vec!["a".into(), "b".into()],
            stopped: vec![],
            unchanged: vec!["c".into()],
        };
        assert_eq!(render_sync(&r), "installed: a, b\nunchanged: c\n");
        assert_eq!(render_sync(&SyncReport::default()), "nothing to do\n");
    }

    #[test]
    fn file_edits_add_once_and_remove_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("balerix").join("plugins.yaml");
        let mut file = load_file(&path).unwrap();
        assert!(file.plugins.is_empty(), "missing file is empty");
        let entry = PluginEntry {
            name: "web".into(),
            source: "/pkg/web.tar.gz".into(),
            sha256: Some("ab".into()),
            secrets: Default::default(),
            config: serde_json::json!({}),
        };
        install_entry(&mut file, entry.clone()).unwrap();
        assert_eq!(
            install_entry(&mut file, entry).unwrap_err().to_string(),
            "plugin \"web\" is already declared in plugins.yaml"
        );
        save_file(&path, &file).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("name: web") && text.contains("sha256: ab"),
            "{text}"
        );
        let mut again = load_file(&path).unwrap();
        assert!(remove_entry(&mut again, "web"));
        assert!(!remove_entry(&mut again, "web"));
        assert!(again.plugins.is_empty());
        // the rewrite is atomic: the previous content is fully replaced and
        // no sibling temp file survives to be mistaken for the real file
        save_file(&path, &again).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("web"));
        let left: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["plugins.yaml"], "no temp file left behind");
    }

    /// An entry's `config` may carry an inline `password` (spec G §4.1), so
    /// the file every `plugin install`/`plugin remove` rewrites must not be
    /// readable by group or others — and a rewrite must not widen a file the
    /// operator already locked down.
    #[test]
    fn the_written_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("balerix").join("plugins.yaml");
        let mut file = PluginsFile::default();
        install_entry(
            &mut file,
            PluginEntry {
                name: "matrix".into(),
                source: "/pkg/matrix".into(),
                sha256: None,
                secrets: Default::default(),
                config: serde_json::json!({ "password": "hunter2" }),
            },
        )
        .unwrap();
        for _ in 0..2 {
            save_file(&path, &file).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "plugins.yaml is mode {mode:04o}");
        }
    }

    /// `--purge` on an entry that is already gone still reaches the daemon:
    /// the state and packages it left behind are what the operator asked to
    /// delete. Only a plain `remove` of an undeclared plugin is an error.
    #[test]
    fn only_a_plain_remove_of_an_undeclared_plugin_is_an_error() {
        assert!(should_bail(false, false));
        assert!(!should_bail(false, true));
        assert!(!should_bail(true, false));
        assert!(!should_bail(true, true));
    }
}
