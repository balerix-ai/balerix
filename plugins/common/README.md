# balerix-plugin-common

Shared building blocks for [balerix](https://github.com/balerix-ai/balerix)
chat plugins: rendering hook events as markdown, splitting long bodies,
showing and answering Claude's `AskUserQuestion` dialogs, config helpers,
a drop-oldest command queue, the shared metric families and phase diffing.
The matrix plugin is built on it; so is the GitHub plugin. This crate pairs
with the `balerix-plugin-sdk` and `balerix-api` versions its manifest names.

The wire contract a plugin speaks is
[`docs/plugin-protocol.md`](https://github.com/balerix-ai/balerix/blob/main/docs/plugin-protocol.md).

## An observer plugin in one page

A plugin has three parts: a `Plugin` impl that validates and enqueues, an
actor task that owns every piece of state, and a channel adapter. The
first two are almost entirely this crate.

```rust
use std::sync::Arc;

use balerix_api::HookEvent;
use balerix_plugin_common::answer::{self, Decision};
use balerix_plugin_common::config::{EventFilter, deserialize, ConfigError};
use balerix_plugin_common::metrics::Shared;
use balerix_plugin_common::pending::{Questions, Stage};
use balerix_plugin_common::phases::PhaseChange;
use balerix_plugin_common::queue::{Health, Queue};
use balerix_plugin_common::{question, render};
use balerix_plugin_sdk::{Host, Metrics, Plugin};
use serde_json::Value;

/// The per-agent block: `plugins.<name>` in the fleet file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct AgentConfig {
    enabled: bool,
    events: EventFilter,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { enabled: true, events: EventFilter::default() }
    }
}

fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    Ok(c)
}

enum Command {
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Reply { agent: String, body: String },
}

struct MyPlugin {
    metrics: Metrics,
    health: Health,
    queue: Arc<Queue<Command>>,
}

impl Plugin for MyPlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let config = parse_agent(&config).map_err(|e| e.to_string())?;
        self.queue.push(Command::Activate { agent: agent.to_string(), config });
        Ok(())
    }
    async fn deactivate(&self, agent: &str) {
        self.queue.push(Command::Deactivate { agent: agent.to_string() });
    }
    async fn observe(&self, events: Vec<HookEvent>) {
        self.queue.push(Command::Events(events));
    }
    async fn health(&self) -> Result<(), String> {
        self.health.get()
    }
    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}

/// What the actor does with one hook event: track questions first, then
/// render whatever the agent's filter wants.
fn on_event(questions: &mut Questions, config: &AgentConfig, event: &HookEvent) -> Option<String> {
    if let Some(input) = event.payload.get("tool_input")
        && event.name == "PreToolUse"
        && event.payload.get("tool_name").and_then(Value::as_str) == Some(question::TOOL)
        && let Some(parsed) = question::parse(input)
    {
        // Persisting through `Questions::open` needs a `Host`; the actor
        // holds one. Rendering is pure:
        return Some(render::question_message(&parsed));
    }
    config.enabled.then(|| config.events.wants(&event.name)).unwrap_or(false)
        .then(|| render::event_message(event))
}

/// What the actor does with a reply while a question is open: decide here,
/// then post `d.post`, send `d.send`, commit `d.stage`, react `d.react`,
/// in that order and under the contract in `Decision`'s docs. The echo id
/// is written back through `Stage::with_echo` only when
/// `Decision::gates_on_post()` is true — a refusal posted while keys are
/// already on their way must not overwrite the saved echo.
fn on_reply(open: &balerix_plugin_common::pending::OpenQuestion, body: &str) -> Decision {
    answer::on_reply(open, body, balerix_api::DEFAULT_KEY_DELAY_MS)
}

fn main() {
    let metrics = Metrics::new("mine");
    let shared = Shared::new(&metrics).expect("fresh registry");
    let queue: Arc<Queue<Command>> = Queue::new(shared.events_dropped.clone());
    let _plugin = MyPlugin { metrics, health: Health::new(), queue };
    // `balerix_plugin_sdk::serve(&host, env!("CARGO_PKG_VERSION"), plugin)`
    // says hello and serves; spawn the actor and
    // `balerix_plugin_common::phases::run(host, |c| queue.push(Command::Phases(c)))`
    // beside it.
    let _ = (on_event, on_reply, Stage::Open, None::<&Host>);
}
```

Long bodies: `render::split(text, limit, max_parts)` breaks at line
boundaries, reopens code fences across parts and marks each part `(n/N)`.
Reviews: `review::render_message` turns a `review::Review` into the one
message the web plugin and the GitHub plugin deliver.

## Testing your plugin

`balerix_plugin_sdk::testing::{FakeHost, Harness}` serve the real router
over HTTP. `question::fixtures` holds dialogs to build tests on. Write your
channel as a port trait with a recording fake, as the matrix plugin's
`matrix.rs` does, and test the actor against it.
