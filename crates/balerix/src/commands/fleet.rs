//! `up`, `update`, `down`, `status`, `list` (Phase 3 spec §5): thin
//! wrappers over `Client` plus one renderer they all share.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{
    ActivationState, AgentPhase, AgentStatus, CredentialBundle, DownQuery, FleetPhase,
    FleetRequest, FleetSpec, FleetSummary,
};
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};
use balerix_core::{Fleet, FleetRecord};

use crate::cli::{ApplyArgs, DownArgs, ListArgs, StatusArgs};
use crate::client::Client;

const POLL: Duration = Duration::from_secs(1);

/// `90`, `90s`, `5m`, `1h`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow!("invalid duration {s:?} (use 90s, 5m or 1h)"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => bail!("invalid duration {s:?} (use 90s, 5m or 1h)"),
    };
    Ok(Duration::from_secs(n * mult))
}

fn label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `flow=active,web=pending`, sorted by name (the map is a `BTreeMap`);
/// empty when the agent has no plugins.
fn plugins_cell(a: &AgentStatus) -> String {
    a.plugins
        .iter()
        .map(|(name, p)| format!("{name}={}", label(p.state)))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn render_status(r: &FleetRecord) -> String {
    let mut out = format!(
        "{}  {}  generation {} (observed {})\n",
        r.name(),
        label(r.status.phase),
        r.generation,
        r.status.observed_generation
    );
    let rows: Vec<[String; 5]> = r
        .status
        .agents
        .iter()
        .map(|(id, a)| {
            [
                id.clone(),
                label(a.phase),
                a.restarts.to_string(),
                plugins_cell(a),
                a.message.clone(),
            ]
        })
        .collect();
    out.push_str(&table(
        &["AGENT", "PHASE", "RESTARTS", "PLUGINS", "MESSAGE"],
        &rows,
    ));
    out
}

pub fn render_list(rows: &[FleetSummary]) -> String {
    if rows.is_empty() {
        return "no fleets\n".to_string();
    }
    let rows: Vec<[String; 5]> = rows
        .iter()
        .map(|s| {
            [
                s.name.clone(),
                label(s.phase),
                s.generation.to_string(),
                s.observed_generation.to_string(),
                s.agents.to_string(),
            ]
        })
        .collect();
    table(&["NAME", "PHASE", "GEN", "OBSERVED", "AGENTS"], &rows)
}

/// `up`/`update` are done when the fleet is Ready and every plugin
/// activation is active (plugins spec §16.3). A rejected activation is an
/// error right away, in the same config-path style the daemon uses.
pub(crate) fn ready_check(r: &FleetRecord) -> Result<bool> {
    for (id, a) in &r.status.agents {
        for (name, p) in &a.plugins {
            if p.state == ActivationState::Rejected {
                let (crew, agent) = id
                    .rsplit_once('/')
                    .and_then(|(rest, agent)| rest.rsplit_once('/').map(|(_, crew)| (crew, agent)))
                    .unwrap_or(("?", id.as_str()));
                bail!("crews.{crew}.agents.{agent}.plugins.{name}: {}", p.message);
            }
        }
    }
    Ok(r.status.observed_generation == r.generation
        && r.status.phase == FleetPhase::Ready
        && r.status.agents.values().all(|a| {
            a.plugins
                .values()
                .all(|p| p.state == ActivationState::Active)
        }))
}

/// Columns padded to the widest cell, two spaces apart, trailing spaces
/// trimmed, so rows compare byte-for-byte in tests.
pub(crate) fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(&format!("{c:<width$}", width = widths[i]));
        }
        s.trim_end_matches(' ').to_string() + "\n"
    };
    let mut out = line(header);
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        out.push_str(&line(&cells));
    }
    out
}

fn load_request(args: &ApplyArgs) -> Result<(FleetSpec, CredentialBundle)> {
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
    // client-side validation before any request (architecture spec §9)
    Fleet::try_from(spec.clone())?;
    Ok((spec, defaults.credentials))
}

/// Polls every second, printing one line per agent phase change to
/// stderr, until `done` or the deadline.
fn wait_until(
    client: &Client,
    name: &str,
    timeout: Duration,
    what: &str,
    done: impl Fn(&FleetRecord) -> Result<bool>,
) -> Result<String> {
    let start = Instant::now();
    let mut seen: BTreeMap<String, AgentPhase> = BTreeMap::new();
    loop {
        let record = client
            .get(name)?
            .ok_or_else(|| anyhow!("fleet {name} disappeared while waiting"))?;
        for (id, a) in &record.status.agents {
            if seen.get(id) != Some(&a.phase) {
                if a.message.is_empty() {
                    eprintln!("{id}: {}", label(a.phase));
                } else {
                    eprintln!("{id}: {} ({})", label(a.phase), a.message);
                }
                seen.insert(id.clone(), a.phase);
            }
        }
        if done(&record)? {
            return Ok(render_status(&record));
        }
        if start.elapsed() >= timeout {
            bail!(
                "timed out after {}s waiting for {what}:\n{}",
                timeout.as_secs(),
                render_status(&record).trim_end()
            );
        }
        std::thread::sleep(POLL);
    }
}

fn apply(args: &ApplyArgs, replace: bool) -> Result<String> {
    let timeout = parse_duration(&args.timeout)?;
    let (spec, credentials) = load_request(args)?;
    let client = Client::connect(args.api_url.as_deref())?;
    let name = spec.name.clone();
    let req = FleetRequest { spec, credentials };
    let record = if replace {
        client.update(&req)?
    } else {
        client.create(&req)?
    };
    if args.no_wait {
        return Ok(render_status(&record));
    }
    wait_until(&client, &name, timeout, "ready", ready_check)
}

pub fn up_command(args: &ApplyArgs) -> Result<String> {
    apply(args, false)
}

pub fn update_command(args: &ApplyArgs) -> Result<String> {
    apply(args, true)
}

pub fn down_command(args: &DownArgs) -> Result<String> {
    let timeout = parse_duration(&args.timeout)?;
    let keep_any = args.keep || args.keep_repos || args.keep_sessions;
    if args.purge && keep_any {
        bail!("--purge cannot be combined with --keep, --keep-repos or --keep-sessions");
    }
    let q = DownQuery {
        keep_repos: args.keep || args.keep_repos,
        keep_sessions: args.keep || args.keep_sessions,
        purge: args.purge,
    };
    let client = Client::connect(args.api_url.as_deref())?;
    client.down(&args.fleet, &q)?;
    if args.purge {
        let start = Instant::now();
        while client.get(&args.fleet)?.is_some() {
            if start.elapsed() >= timeout {
                bail!(
                    "timed out after {}s waiting for the purge",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(POLL);
        }
        return Ok(format!("{}: purged\n", args.fleet));
    }
    wait_until(&client, &args.fleet, timeout, "down", |r| {
        Ok(r.status.phase == FleetPhase::Down)
    })
}

pub fn status_command(args: &StatusArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let record = client
        .get(&args.fleet)?
        .with_context(|| format!("fleet {} not found", args.fleet))?;
    Ok(if args.json {
        serde_json::to_string_pretty(&record)? + "\n"
    } else {
        render_status(&record)
    })
}

pub fn list_command(args: &ListArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let rows = client.list()?;
    Ok(if args.json {
        serde_json::to_string_pretty(&rows)? + "\n"
    } else {
        render_list(&rows)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentPhase, FleetPhase, FleetSpec, PluginActivation};
    use std::collections::BTreeMap;

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn status_and_list_render_aligned_tables() {
        let mut r = FleetRecord::new(FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::new(),
            ..Default::default()
        });
        r.generation = 3;
        r.status.generation = 3;
        r.status.observed_generation = 3;
        r.status.phase = FleetPhase::Degraded;
        let alice = r.status.entry("payments/backend/alice");
        alice.phase = AgentPhase::Ready;
        alice
            .plugins
            .insert("flow".into(), PluginActivation::active());
        let bob = r.status.entry("payments/backend/bob");
        bob.phase = AgentPhase::Starting;
        bob.restarts = 1;
        bob.message = "exited with status 1".into();
        bob.plugins
            .insert("flow".into(), PluginActivation::rejected("bad regex"));
        bob.plugins
            .insert("web".into(), PluginActivation::pending());
        assert_eq!(
            render_status(&r),
            "payments  degraded  generation 3 (observed 3)\n\
             AGENT                   PHASE     RESTARTS  PLUGINS                    MESSAGE\n\
             payments/backend/alice  ready     0         flow=active\n\
             payments/backend/bob    starting  1         flow=rejected,web=pending  exited with status 1\n"
        );
        let rows = vec![r.summary()];
        assert_eq!(
            render_list(&rows),
            "NAME      PHASE     GEN  OBSERVED  AGENTS\n\
             payments  degraded  3    3         2\n"
        );
        assert_eq!(render_list(&[]), "no fleets\n");
    }

    #[test]
    fn ready_needs_every_activation_active_and_a_rejection_fails() {
        let mut r = FleetRecord::new(FleetSpec {
            name: "p".into(),
            crews: BTreeMap::new(),
            ..Default::default()
        });
        r.generation = 1;
        r.status.generation = 1;
        r.status.observed_generation = 1;
        r.status.phase = FleetPhase::Ready;
        r.status.entry("p/c/a").phase = AgentPhase::Ready;
        assert!(ready_check(&r).unwrap());
        r.status
            .entry("p/c/a")
            .plugins
            .insert("flow".into(), PluginActivation::pending());
        assert!(!ready_check(&r).unwrap(), "pending holds");
        r.status
            .entry("p/c/a")
            .plugins
            .insert("flow".into(), PluginActivation::active());
        assert!(ready_check(&r).unwrap());
        r.status.entry("p/c/a").plugins.insert(
            "web".into(),
            PluginActivation::rejected("initial: unknown state \"x\""),
        );
        assert_eq!(
            ready_check(&r).unwrap_err().to_string(),
            "crews.c.agents.a.plugins.web: initial: unknown state \"x\""
        );
        r.status.phase = FleetPhase::Reconciling;
        r.status.entry("p/c/a").plugins.clear();
        assert!(!ready_check(&r).unwrap());
    }
}
