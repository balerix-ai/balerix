//! `balerix dev …` (Phase 2 spec §5; `fake-claude` is Phase 3 spec §7).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};
use balerix_core::{Fleet, HookTarget, ResolvedAgent};
use balerix_plugin_sdk::Host;
use balerix_runtime::{RenderOptions, Runtime, StateLayout};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::{FakeClaudeArgs, MaterializeArgs};
use crate::wiring::{layout_from_env, tool_paths};

pub fn materialize_command(args: &MaterializeArgs) -> Result<String> {
    if args.agent.split('/').count() != 2 {
        bail!("agent must be crew/agent");
    }
    let file = read(&args.file)?;
    let defaults = if args.no_host_defaults {
        host::HostDefaults::default()
    } else {
        host::load(&HostPaths::discover()?)?
    };
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: args.name.clone(),
            host_claude_settings: defaults.claude_settings,
        },
    )?;
    let fleet = Fleet::try_from(spec)?;
    let wanted = format!("{}/{}", fleet.name, args.agent);
    let agent = ResolvedAgent::from_fleet(&fleet)
        .into_iter()
        .find(|a| a.id.to_string() == wanted)
        .with_context(|| format!("no agent {wanted:?} in the fleet (expected crew/agent)"))?;

    let real = layout_from_env()?;
    let tools = tool_paths()?;

    // Only claim (`.keep()`) the fallback temp dir after everything below
    // succeeds; on an early return via `?` this drops and cleans up.
    let mut fallback_tmp = None;
    let out_root = match &args.out {
        Some(p) => p.clone(),
        None => {
            let dir = tempfile::Builder::new()
                .prefix("balerix-materialize-")
                .tempdir()?;
            let path = dir.path().to_path_buf();
            fallback_tmp = Some(dir);
            path
        }
    };
    let layout = StateLayout {
        state_root: out_root.join("state"),
        data_root: out_root.join("data"),
        config_root: real.config_root,
    };
    let rt = Runtime::new(layout.clone(), tools);

    let paths = layout.agent(&agent.id);
    std::fs::create_dir_all(&paths.workspace)?;
    std::fs::create_dir_all(layout.crew(&agent.id.crew_ref()).repo.join(".git"))?;
    let hooks = HookTarget {
        url: args.hooks_url.clone(),
        secret: throwaway_secret(),
    };
    rt.render_agent(
        &agent,
        &defaults.credentials,
        &hooks,
        &RenderOptions {
            redact_credentials: !args.with_credentials,
        },
    )?;
    if args.install {
        rt.install_and_validate(&agent)?;
    }
    if let Some(dir) = fallback_tmp {
        let _ = dir.keep();
    }

    let mut out = format!("agent dir: {}\n", paths.root.display());
    let credentials_path = paths.claude_dir().join(".credentials.json");
    let hosts_path = paths.gh_dir().join("hosts.yml");
    for (label, p) in [
        ("settings.json", paths.claude_dir().join("settings.json")),
        (".credentials.json", credentials_path.clone()),
        ("hosts.yml", hosts_path.clone()),
        ("mise.toml", paths.mise_toml.clone()),
        ("nono-profile.json", paths.profile.clone()),
        ("launch.sh", paths.launch.clone()),
    ] {
        if p.exists() {
            out.push_str(&format!("  {label:<19} {}\n", p.display()));
        }
    }
    out.push_str(if args.with_credentials {
        "credentials: written (real)\n"
    } else if credentials_path.exists() || hosts_path.exists() {
        "credentials: <redacted> placeholders (pass --with-credentials to write them)\n"
    } else {
        "credentials: none found on the host (nothing to redact)\n"
    });
    if !args.install {
        out.push_str("not run: mise install, nono profile validate (pass --install)\n");
    }
    Ok(out)
}

/// Not a real secret: only so the rendered settings.json has the right shape.
fn throwaway_secret() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(Sha256::digest(format!("{}-{nanos}", std::process::id())))[..32].to_string()
}

/// `balerix dev fake-claude` (Phase 3 spec §7): what the e2e launches in
/// place of `claude`. Runs once, then sleeps until the runner kills it —
/// which keeps the stdin pump alive for as long as the process lives.
pub fn fake_claude_command(args: &FakeClaudeArgs) -> Result<String> {
    let config_dir =
        PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").context("CLAUDE_CONFIG_DIR not set")?);
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    let _pump = fake_claude_once(&config_dir, &home, &args.rest, std::io::stdin())?;
    eprintln!("fake-claude: hooks done; sleeping until killed");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Reads `input` line by line until EOF, appending each line to `out`.
/// This is how the e2e sees what tmux `send-keys` delivered.
pub fn pump_stdin(input: impl Read + Send + 'static, out: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(input);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&out)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    })
}

/// Posts `payload` to every HTTP hook declared for `event`, returning the
/// last reply body. Failures are printed, not fatal: the fake is a probe.
fn post_hook(settings: &Value, event: &str, payload: &Value) -> Option<String> {
    let mut reply = None;
    for hook in hooks_of(settings, event) {
        let Some(url) = (hook["type"] == "http")
            .then(|| hook["url"].as_str())
            .flatten()
        else {
            continue;
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent.post(url).header("Content-Type", "application/json");
        if let Some(headers) = hook["headers"].as_object() {
            for (k, v) in headers {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        }
        // A raw compact body, not `send_json` (which pretty-prints), to
        // match the payload shape Claude actually sends.
        match req.send(payload.to_string().as_bytes()) {
            Ok(mut r) => {
                let status = r.status();
                let body = r.body_mut().read_to_string().unwrap_or_default();
                eprintln!("fake-claude: {event} hook → {status}");
                reply = Some(body);
            }
            Err(e) => eprintln!("fake-claude: {event} hook failed: {e}"),
        }
    }
    reply
}

/// Records argv, marks a session so `--continue` triggers next time, runs
/// every `SessionStart` command hook with a payload on stdin (as Claude
/// does), starts the stdin pump, then posts `Notification`, `PreToolUse`
/// and `Stop` to their HTTP hooks, recording the interceptable replies.
/// The returned handle owns the pump thread.
pub fn fake_claude_once(
    config_dir: &Path,
    home: &Path,
    argv: &[String],
    stdin: impl Read + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    std::fs::write(home.join("fake-claude.argv"), argv.join("\n") + "\n")?;
    let projects = config_dir.join("projects").join("e2e");
    std::fs::create_dir_all(&projects)?;
    std::fs::write(projects.join("session.marker"), "fake\n")?;
    let settings: Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("settings.json"))?)?;
    let cwd = std::env::current_dir()?;

    for hook in hooks_of(&settings, "SessionStart") {
        let Some(cmd) = (hook["type"] == "command")
            .then(|| hook["command"].as_str())
            .flatten()
        else {
            continue;
        };
        let payload = json!({
            "hook_event_name": "SessionStart", "session_id": "fake-claude",
            "cwd": cwd, "source": "startup"
        });
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot run SessionStart hook {cmd:?}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.to_string().as_bytes())?;
        }
        let out = child.wait_with_output()?;
        eprintln!(
            "fake-claude: SessionStart hook {cmd:?} exited {} with {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }

    let pump = pump_stdin(stdin, home.join("fake-claude.stdin"));

    post_hook(
        &settings,
        "Notification",
        &json!({
            "hook_event_name": "Notification", "session_id": "fake-claude",
            "message": "fake claude is up"
        }),
    );
    // The two the plugin chain intercepts (plugins spec §16.5): the reply
    // body is the merged verdict, so the e2e reads it back off disk.
    for (event, payload) in [
        (
            "PreToolUse",
            json!({
                "hook_event_name": "PreToolUse", "session_id": "fake-claude",
                "tool_name": "Bash", "tool_input": { "command": "rm -rf /tmp/x" }
            }),
        ),
        (
            "Stop",
            json!({
                "hook_event_name": "Stop", "session_id": "fake-claude",
                "stop_hook_active": false
            }),
        ),
    ] {
        if let Some(reply) = post_hook(&settings, event, &payload) {
            std::fs::write(home.join(format!("fake-claude.{event}.reply")), reply)?;
        }
    }
    Ok(pump)
}

fn hooks_of(settings: &Value, event: &str) -> Vec<Value> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["hooks"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// `balerix dev fake-plugin`: the e2e's plugin, on the full SDK. Blocks
/// `rm -rf` at PreToolUse, answers Stop with a send_text, observes
/// everything into scratch, records activations and the hello reply.
/// Binding a loopback listener under nono is plugins spec §11.1 row 1: if
/// it is refused, the verdict is left in scratch instead of hanging.
pub fn fake_plugin_command() -> Result<String> {
    use balerix_plugin_sdk::{Env, bind, run};
    let env = Env::from_process()?;
    let scratch = env.scratch.clone();
    std::fs::create_dir_all(&scratch)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let token = env.token.clone();
        let host = Host::new(env)?;
        let (listener, listen) = match bind().await {
            Ok(b) => b,
            Err(e) => {
                std::fs::write(scratch.join("fake-plugin.bind-failed"), e.to_string())?;
                bail!("fake-plugin: cannot bind a loopback listener: {e}");
            }
        };
        let plugin = Arc::new(FakePlugin {
            scratch: scratch.clone(),
        });
        let server = tokio::spawn(async move { run(listener, plugin, &token).await });
        let resp = host.hello(env!("CARGO_PKG_VERSION"), &listen).await?;
        std::fs::write(
            scratch.join("fake-plugin.hello"),
            serde_json::to_string_pretty(&resp.config)?,
        )?;
        eprintln!("fake-plugin: hello acknowledged; listening on {listen}");
        manage_from_config(&host, &resp.config, &scratch).await?;
        server.await??;
        Ok::<String, anyhow::Error>(String::new())
    })
}

/// Spec L: `config.manage = { fleet, file }` in the plugin's `plugins.yaml`
/// entry makes the fake apply that fleet file once it is up — how the e2e
/// exercises a plugin-managed fleet. The outcome, the record or
/// `{ "error" }`, lands in `scratch/fake-plugin.manage`; a refusal is not
/// fatal, the journey reads it.
async fn manage_from_config(host: &Host, config: &Value, scratch: &Path) -> Result<()> {
    let Some(manage) = config.get("manage") else {
        return Ok(());
    };
    let outcome = match (manage["fleet"].as_str(), manage.get("file")) {
        (Some(fleet), Some(file)) => match host.apply_fleet(fleet, file).await {
            Ok(record) => serde_json::to_value(record)?,
            Err(e) => json!({ "error": e.to_string() }),
        },
        _ => json!({ "error": "config.manage needs `fleet` and `file`" }),
    };
    std::fs::write(
        scratch.join("fake-plugin.manage"),
        serde_json::to_string_pretty(&outcome)?,
    )?;
    eprintln!("fake-plugin: manage outcome written");
    Ok(())
}

struct FakePlugin {
    scratch: PathBuf,
}

impl FakePlugin {
    /// One JSON document per line. Best effort: a plugin that cannot write
    /// its observations still has to answer the daemon.
    fn append(&self, file: &str, line: &Value) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.scratch.join(file))
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

impl balerix_plugin_sdk::Plugin for FakePlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        if config.get("reject").is_some() {
            return Err(format!("fake-plugin: rejected by config for {agent}"));
        }
        self.append(
            "activations.jsonl",
            &json!({ "agent": agent, "config": config }),
        );
        Ok(())
    }

    async fn observe(&self, events: Vec<balerix_api::HookEvent>) {
        for e in events {
            if let Ok(v) = serde_json::to_value(&e) {
                self.append("events.jsonl", &v);
            }
        }
    }

    async fn intercept(
        &self,
        event: balerix_api::HookEvent,
        mut so_far: Value,
        _deadline_ms: u64,
    ) -> balerix_api::InterceptResponse {
        let mut actions = Vec::new();
        match event.name.as_str() {
            "PreToolUse"
                if event.payload["tool_input"]["command"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("rm -rf")) =>
            {
                so_far["decision"] = json!("block");
                so_far["reason"] = json!("fake-plugin: no recursive deletes");
            }
            "Stop" => actions.push(balerix_api::PluginAction::SendText {
                text: "fake-plugin says hi".into(),
                submit: true,
            }),
            _ => {}
        }
        balerix_api::InterceptResponse {
            response: so_far,
            actions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stub_server_n;
    use serde_json::json;

    /// Spec L: `config.manage` in the fake's `plugins.yaml` entry is what
    /// the e2e uses to make a plugin apply a fleet; the outcome lands in
    /// scratch for the journey to read.
    #[test]
    fn the_fake_plugin_applies_the_fleet_its_config_names() {
        use balerix_plugin_sdk::testing::FakeHost;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let fake = FakeHost::start("tok", json!({}), vec![]).await;
            let host = balerix_plugin_sdk::Host::new(fake.env("fake", Path::new("/s"))).unwrap();
            let scratch = tempfile::tempdir().unwrap();
            // no `manage`: nothing happens, no file
            manage_from_config(&host, &json!({ "greeting": "hi" }), scratch.path())
                .await
                .unwrap();
            assert!(!scratch.path().join("fake-plugin.manage").exists());
            assert!(fake.applied_fleets().is_empty());
            // `manage`: the file is applied and the record recorded
            let file = json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": {} });
            manage_from_config(
                &host,
                &json!({ "manage": { "fleet": "managed", "file": file } }),
                scratch.path(),
            )
            .await
            .unwrap();
            let outcome: Value = serde_json::from_str(
                &std::fs::read_to_string(scratch.path().join("fake-plugin.manage")).unwrap(),
            )
            .unwrap();
            assert_eq!(outcome["spec"]["name"], "managed");
            assert_eq!(outcome["owner"], "plugin");
            assert_eq!(fake.applied_fleets(), vec![("managed".to_string(), file)]);
            // a refusal is recorded too, not fatal
            fake.fail_manage(Some((400, "name: \"x\" does not match the fleet managed")));
            manage_from_config(
                &host,
                &json!({ "manage": { "fleet": "managed", "file": {} } }),
                scratch.path(),
            )
            .await
            .unwrap();
            let outcome: Value = serde_json::from_str(
                &std::fs::read_to_string(scratch.path().join("fake-plugin.manage")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                outcome["error"],
                "daemon: HTTP 400: name: \"x\" does not match the fleet managed"
            );
        });
    }

    #[test]
    fn fake_claude_runs_command_hooks_posts_three_http_hooks_records_replies_and_reads_stdin() {
        let (url, seen) = stub_server_n(3, "200 OK", "{\"ok\":true}");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config_dir = home.join(".claude");
        std::fs::create_dir_all(&config_dir).unwrap();
        let http = json!([{ "hooks": [{ "type": "http", "url": format!("{url}/v1/agents/f/c/a/events"), "headers": { "Authorization": "Bearer s3" } }] }]);
        let settings = json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "cat > \"$HOME/seen.json\"", "timeout": 10 }] }],
                "Notification": http,
                "PreToolUse": http,
                "Stop": http
            }
        });
        std::fs::write(config_dir.join("settings.json"), settings.to_string()).unwrap();
        let pump = fake_claude_once(
            &config_dir,
            &home,
            &["--verbose".into(), "--continue".into()],
            std::io::Cursor::new("hello there\nsecond line\n"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join("fake-claude.argv")).unwrap(),
            "--verbose\n--continue\n"
        );
        let seen_payload: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("seen.json")).unwrap())
                .unwrap();
        assert_eq!(seen_payload["hook_event_name"], "SessionStart");
        assert!(config_dir.join("projects/e2e/session.marker").exists());

        let notification = seen.recv().unwrap();
        assert!(
            notification.contains("Authorization: Bearer s3")
                || notification.contains("authorization: Bearer s3"),
            "{notification}"
        );
        assert!(
            notification.contains(r#""hook_event_name":"Notification""#),
            "{notification}"
        );
        let pre = seen.recv().unwrap();
        assert!(pre.contains(r#""hook_event_name":"PreToolUse""#), "{pre}");
        let body: Value = serde_json::from_str(pre.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["tool_name"], "Bash");
        assert!(
            body["tool_input"]["command"]
                .as_str()
                .unwrap()
                .starts_with("rm -rf"),
            "{body}"
        );
        let stop = seen.recv().unwrap();
        assert!(stop.contains(r#""hook_event_name":"Stop""#), "{stop}");

        for event in ["PreToolUse", "Stop"] {
            let reply =
                std::fs::read_to_string(home.join(format!("fake-claude.{event}.reply"))).unwrap();
            assert_eq!(reply, "{\"ok\":true}", "{event}");
        }

        // the pump thread ran to EOF on the injected stdin
        pump.join().unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("fake-claude.stdin")).unwrap(),
            "hello there\nsecond line\n"
        );
    }
}
