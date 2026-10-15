//! `balerix dev …` (Phase 2 spec §5; `fake-claude` is Phase 3 spec §7).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};
use balerix_core::{Fleet, HookTarget, ResolvedAgent};
use balerix_plugin_sdk::{Host, SdkError};
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
            ..ResolveOptions::default()
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
    let layout = StateLayout::xdg(
        out_root.join("state"),
        out_root.join("data"),
        real.config_root,
    );
    // what `serve` passes, so the preview holds the same grants off
    // the daemon's tmux socket directory (#187)
    let rt = Runtime::new(layout.clone(), tools)
        .with_tmux_tmpdir(std::env::var_os("TMUX_TMPDIR").map(std::path::PathBuf::from));

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
    out.push_str(
        "sandbox: previewed with unix_sockets = \"open\" (serve resolves its own policy)\n",
    );
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
    if args.wrap_pastes {
        // ask the terminal for bracketed paste, as Claude does
        let mut out = std::io::stdout();
        out.write_all(BRACKETED_PASTE_ON.as_bytes())?;
        out.flush()?;
    }
    let _pump = fake_claude_once(
        &config_dir,
        &home,
        &args.rest,
        std::io::stdin(),
        args.wrap_pastes,
    )?;
    eprintln!("fake-claude: hooks done; sleeping until killed");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// What `--wrap-pastes` makes of the pane's input (#279): the turns
/// Claude Code 2.1.283+ would submit. Outside a bracketed paste a
/// newline (a cooked tty's Enter) submits the turn so far, if any;
/// inside one it is text. A paste of more than 800 characters or more
/// than two newlines (what 2.1.299 and 2.1.300 collapse) joins the turn
/// as `\n\n<pasted_content id="…">\n{paste}\n</pasted_content id="…">\n\n`,
/// any other paste as it was. `log` is the input less the paste markers,
/// for `fake-claude.stdin`.
#[derive(Debug, Default)]
pub struct PasteComposer {
    carry: Vec<u8>,
    paste: Option<Vec<u8>>,
    turn: Vec<u8>,
    pastes: u32,
}

impl PasteComposer {
    /// Feeds `bytes`; answers the input less the paste markers and every
    /// turn submitted. A marker split across two feeds waits for the
    /// second.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<String>) {
        self.carry.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.carry);
        let (mut log, mut turns) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            if rest.starts_with(PASTE_START) {
                self.paste = Some(Vec::new());
                i += PASTE_START.len();
                continue;
            }
            if rest.starts_with(PASTE_END) {
                let paste = self.paste.take().unwrap_or_default();
                self.join_paste(&paste);
                i += PASTE_END.len();
                continue;
            }
            if rest.len() < PASTE_START.len()
                && (PASTE_START.starts_with(rest) || PASTE_END.starts_with(rest))
            {
                self.carry = rest.to_vec();
                break;
            }
            let b = rest[0];
            let b = if b == b'\r' { b'\n' } else { b };
            log.push(b);
            match &mut self.paste {
                Some(paste) => paste.push(b),
                None if b == b'\n' => {
                    let turn = std::mem::take(&mut self.turn);
                    if !turn.iter().all(u8::is_ascii_whitespace) {
                        turns.push(String::from_utf8_lossy(&turn).into_owned());
                    }
                }
                None => self.turn.push(b),
            }
            i += 1;
        }
        (log, turns)
    }

    fn join_paste(&mut self, paste: &[u8]) {
        let text = String::from_utf8_lossy(paste);
        let newlines = text.matches('\n').count();
        if text.chars().count() > 800 || newlines > 2 {
            self.pastes += 1;
            let id = format!("{:04x}", 0x7900 + self.pastes);
            let wrapped = format!(
                "\n\n<pasted_content id=\"{id}\">\n{text}\n</pasted_content id=\"{id}\">\n\n"
            );
            self.turn.extend_from_slice(wrapped.as_bytes());
        } else {
            self.turn.extend_from_slice(paste);
        }
    }
}

/// `pump_stdin` for `--wrap-pastes`: `input` through a [`PasteComposer`],
/// the input less the paste markers appended to `out`, each submitted
/// turn to `turns` as a JSON string on a line of its own.
pub fn pump_composer(
    mut input: impl Read + Send + 'static,
    out: PathBuf,
    turns: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut composer = PasteComposer::default();
        let mut buf = [0u8; 8192];
        let append = |path: &Path, bytes: &[u8]| {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(bytes);
            }
        };
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let (log, submitted) = composer.feed(&buf[..n]);
            append(&out, &log);
            for turn in submitted {
                append(&turns, format!("{}\n", Value::String(turn)).as_bytes());
            }
        }
    })
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
    wrap_pastes: bool,
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

    let pump = if wrap_pastes {
        pump_composer(
            stdin,
            home.join("fake-claude.stdin"),
            home.join("fake-claude.turns"),
        )
    } else {
        pump_stdin(stdin, home.join("fake-claude.stdin"))
    };

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
    use balerix_plugin_sdk::Env;
    let env = Env::from_process()?;
    let scratch = env.scratch.clone();
    std::fs::create_dir_all(&scratch)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let host = Host::new(env)?;
        let plugin = FakePlugin {
            scratch: scratch.clone(),
            host: host.clone(),
        };
        match balerix_plugin_sdk::serve(&host, env!("CARGO_PKG_VERSION"), plugin).await {
            Ok(()) => Ok(String::new()),
            Err(SdkError::Bind(e)) => {
                std::fs::write(scratch.join("fake-plugin.bind-failed"), &e)?;
                bail!("fake-plugin: cannot bind its listener: {e}");
            }
            Err(e) => Err(e.into()),
        }
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
            Ok(Some(record)) => serde_json::to_value(record)?,
            Ok(None) => json!({ "applied": true }),
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

/// What the fake would ship as `balerix-plugin.yaml`: the hooks and the
/// capabilities the e2e and the Kubernetes tests exercise.
const FAKE_PLUGIN_MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: fake\nversion: 0.0.0\nprotocol: 1\nstart: serve\nhooks:\n  observe: [SessionStart, Notification, PreToolUse, Stop]\n  intercept: [PreToolUse, Stop]\nneeds: [actions, fleets, kv, manage]\n";

struct FakePlugin {
    scratch: PathBuf,
    host: Host,
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
    fn manifest(&self) -> Option<&'static str> {
        Some(FAKE_PLUGIN_MANIFEST)
    }

    /// The `hello` reply: written to scratch, then `config.manage` applied
    /// off the serving path (an apply may take the SDK's 120 s).
    /// `config.try_escape` first tries to write `escape` into the package
    /// (the working directory) and leaves the outcome in
    /// `scratch/fake-plugin.escape`, before the hello file appears.
    async fn configure(&self, config: Value) -> Result<(), String> {
        if config["try_escape"].as_bool() == Some(true) {
            let outcome = match std::env::current_dir()
                .and_then(|pwd| std::fs::write(pwd.join("escape"), "escaped\n"))
            {
                Ok(()) => "written".to_string(),
                Err(e) => format!("refused: {e}"),
            };
            std::fs::write(self.scratch.join("fake-plugin.escape"), outcome)
                .map_err(|e| e.to_string())?;
        }
        std::fs::write(
            self.scratch.join("fake-plugin.hello"),
            serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        eprintln!("fake-plugin: hello acknowledged");
        let (host, scratch) = (self.host.clone(), self.scratch.clone());
        tokio::spawn(async move {
            if let Err(e) = manage_from_config(&host, &config, &scratch).await {
                eprintln!("fake-plugin: manage: {e}");
            }
        });
        Ok(())
    }

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

    /// #279: a collapsible paste is recorded wrapped, a short one as
    /// typed, and the text typed after it (the runner's lead) stays
    /// outside the wrapper; split markers wait for the rest.
    #[test]
    fn the_composer_wraps_a_collapsible_paste_as_claude_does() {
        let mut c = PasteComposer::default();
        let (log, turns) = c.feed(b"short line\n");
        assert_eq!(
            (log.as_slice(), turns),
            (&b"short line\n"[..], vec!["short line".to_string()])
        );
        // four lines, the second chunk splitting the end marker
        let (_, turns) = c.feed(b"\x1b[200~a\nb\n\nd\x1b[20");
        assert!(turns.is_empty());
        let (log, turns) = c.feed(b"1~ Act on it.\n");
        assert_eq!(
            turns,
            vec!["\n\n<pasted_content id=\"7901\">\na\nb\n\nd\n</pasted_content id=\"7901\">\n\n Act on it.".to_string()]
        );
        assert_eq!(log, b" Act on it.\n".to_vec());
        // three lines arrive as typed; a lone Enter submits nothing
        let (_, turns) = c.feed(b"\x1b[200~x\ny\nz\x1b[201~\n\n");
        assert_eq!(turns, vec!["x\ny\nz".to_string()]);
        // one line over 800 characters is wrapped; 800 is not
        let long = "y".repeat(801);
        let (_, turns) = c.feed(format!("\x1b[200~{long}\x1b[201~\r").as_bytes());
        assert!(
            turns[0].starts_with("\n\n<pasted_content id=\"7902\">\ny"),
            "{turns:?}"
        );
        let (_, turns) = c.feed(format!("\x1b[200~{}\x1b[201~\n", &long[1..]).as_bytes());
        assert_eq!(turns, vec![long[1..].to_string()]);
    }

    /// `scripts/verify-claude.sh` types the lead by hand, as `send_text`
    /// does (#279): the two must not drift apart.
    #[test]
    fn verify_claude_types_the_runners_lead() {
        let script = include_str!("../../../../scripts/verify-claude.sh");
        let line = format!("LEAD=\"{}\"", balerix_api::PASTE_LEAD);
        assert!(script.contains(&line), "{line}");
    }

    #[test]
    fn wrap_pastes_is_the_fakes_own_flag_and_the_rest_is_recorded() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "balerix",
            "dev",
            "fake-claude",
            "--wrap-pastes",
            "--verbose",
            "--continue",
        ])
        .unwrap();
        let crate::cli::Command::Dev {
            command: crate::cli::DevCommand::FakeClaude(args),
        } = cli.command
        else {
            panic!("not fake-claude");
        };
        assert!(args.wrap_pastes);
        assert_eq!(args.rest, ["--verbose", "--continue"]);
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
            false,
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
