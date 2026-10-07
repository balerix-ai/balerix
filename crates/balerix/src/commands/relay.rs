//! `balerix hook-relay` (Phase 3 spec §4): Claude runs this as the
//! `SessionStart` command hook. Reads the hook JSON from stdin, posts it to
//! the daemon with the agent's secret, prints the reply. On any failure it
//! prints `{}` and exits 0 — a daemon outage degrades the fleet, it never
//! breaks the agent.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// The daemon's limit on the events route (`balerix-server`'s `api.rs`).
const MAX_BODY: u64 = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn hook_relay_command() -> Result<String> {
    match relay(std::io::stdin().lock(), &|k| std::env::var(k).ok()) {
        Ok(reply) => Ok(reply),
        Err(e) => {
            eprintln!("balerix hook-relay: {e}");
            Ok("{}\n".to_string())
        }
    }
}

pub fn relay(input: impl Read, env: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let url = env("BALERIX_API_URL").context("BALERIX_API_URL not set")?;
    let id = env("BALERIX_AGENT_ID").context("BALERIX_AGENT_ID not set")?;
    let secret = env("BALERIX_HOOK_SECRET").context("BALERIX_HOOK_SECRET not set")?;
    let mut input = input;
    let mut body = Vec::new();
    (&mut input)
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .context("cannot read the hook event from stdin")?;
    if body.len() as u64 > MAX_BODY {
        // Posted cut short it would be a 400 about bad JSON (#116). The
        // rest is read and dropped so Claude's write does not fail either.
        let _ = std::io::copy(&mut input, &mut std::io::sink());
        bail!(
            "the hook event is over the daemon's {} MiB limit ({MAX_BODY} bytes); not sent",
            MAX_BODY >> 20
        );
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!(
            "{}/v1/agents/{id}/events",
            url.trim_end_matches('/')
        ))
        .header("Authorization", &format!("Bearer {secret}"))
        .header("Content-Type", "application/json")
        .send(&body[..])
        .context("cannot reach the daemon")?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .read_to_string()
        .context("cannot read the daemon's reply")?;
    if !status.is_success() {
        bail!("daemon answered {status}: {}", text.trim());
    }
    Ok(if text.trim().is_empty() {
        "{}\n".to_string()
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stub_server;
    use std::collections::HashMap;

    fn env(url: &str) -> HashMap<&'static str, String> {
        HashMap::from([
            ("BALERIX_API_URL", url.to_string()),
            ("BALERIX_AGENT_ID", "f/c/a".to_string()),
            ("BALERIX_HOOK_SECRET", "s3".to_string()),
        ])
    }

    #[test]
    fn posts_stdin_to_the_agent_route_with_the_secret_and_prints_the_reply() {
        let (url, seen) = stub_server("200 OK", r#"{"ok":true}"#);
        let vars = env(&format!("{url}/"));
        let out = relay(r#"{"hook_event_name":"SessionStart"}"#.as_bytes(), &|k| {
            vars.get(k).cloned()
        })
        .unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
        let req = seen.recv().unwrap().to_ascii_lowercase();
        assert!(
            req.starts_with("post /v1/agents/f/c/a/events http/1.1"),
            "{req}"
        );
        assert!(req.contains("authorization: bearer s3"), "{req}");
        assert!(req.contains("content-type: application/json"), "{req}");
        assert!(
            req.ends_with(r#"{"hook_event_name":"sessionstart"}"#),
            "{req}"
        );
    }

    /// #116: a payload over the daemon's limit is not posted truncated
    /// (which the daemon would answer 400, bad JSON): the relay says why.
    #[test]
    fn an_event_over_the_limit_is_refused_by_the_relay_naming_the_limit() {
        let (url, seen) = stub_server("200 OK", "{}");
        let vars = env(&url);
        let big = vec![b' '; MAX_BODY as usize + 1];
        let e = relay(big.as_slice(), &|k| vars.get(k).cloned())
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the hook event is over the daemon's 1 MiB limit (1048576 bytes); not sent"
        );
        assert!(
            seen.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "nothing was posted"
        );
        // exactly the limit still goes
        let at = vec![b' '; MAX_BODY as usize];
        relay(at.as_slice(), &|k| vars.get(k).cloned()).unwrap();
        let req = seen.recv().unwrap().to_ascii_lowercase();
        assert!(req.contains("content-length: 1048576"), "{}", &req[..200]);
    }

    #[test]
    fn failures_are_errors_the_command_turns_into_an_empty_object() {
        let (url, _seen) = stub_server("503 Service Unavailable", r#"{"error":"nope"}"#);
        let vars = env(&url);
        let e = relay(b"{}".as_slice(), &|k| vars.get(k).cloned())
            .unwrap_err()
            .to_string();
        assert!(e.contains("503"), "{e}");
        let vars = env("http://127.0.0.1:1");
        assert!(relay(b"{}".as_slice(), &|k| vars.get(k).cloned()).is_err());
        let e = relay(b"{}".as_slice(), &|_| None).unwrap_err().to_string();
        assert!(e.contains("BALERIX_API_URL"), "{e}");
    }
}
