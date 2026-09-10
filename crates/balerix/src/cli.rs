//! Command-line surface. Every subcommand is a thin wrapper (spec §3):
//! parse args, call a library, print.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "balerix",
    version,
    about = "Control plane and orchestrator for fleets of coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon (foreground, or detached with -d).
    Serve(ServeArgs),
    /// Inspect fleet configuration without talking to the daemon.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Developer tools; not part of the supported surface.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
    /// Create a fleet from a YAML file and wait until it is ready.
    Up(ApplyArgs),
    /// Replace a running fleet's spec; only agents whose settings changed restart.
    Update(ApplyArgs),
    /// Stop a fleet; keep repos and/or sessions, or purge everything.
    Down(DownArgs),
    /// Show one fleet.
    Status(StatusArgs),
    /// List fleets.
    List(ListArgs),
    /// Manage daemon plugins declared in $XDG_CONFIG_HOME/balerix/plugins.yaml.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },

    // -- internal --
    /// Reads the SessionStart hook JSON on stdin, posts it to the daemon,
    /// prints the reply; on any failure prints `{}` and exits 0.
    #[command(hide = true)]
    HookRelay,
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json or send host credentials.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// How long to wait for Ready (e.g. 90s, 5m, 1h).
    #[arg(long, default_value = "5m")]
    pub timeout: String,
    /// Return right after the request instead of waiting for Ready.
    #[arg(long)]
    pub no_wait: bool,
    /// Daemon URL (default: $BALERIX_API_URL, then the running daemon's endpoint file).
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct DownArgs {
    pub fleet: String,
    #[arg(long)]
    pub keep_repos: bool,
    #[arg(long)]
    pub keep_sessions: bool,
    /// Both --keep-repos and --keep-sessions.
    #[arg(long)]
    pub keep: bool,
    /// Also delete the fleet record and everything under its directory.
    #[arg(long)]
    pub purge: bool,
    #[arg(long, default_value = "5m")]
    pub timeout: String,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub fleet: String,
    /// Print the raw record as JSON.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Add a package (directory, tarball or https URL) to plugins.yaml and sync a running daemon.
    Install(PluginInstallArgs),
    /// Reconcile the running daemon to plugins.yaml.
    Sync(ApiOnlyArgs),
    /// List declared plugins with phase and listen address.
    List(ListArgs),
    /// Remove a plugin from plugins.yaml; --purge also deletes its state and packages.
    Remove(PluginRemoveArgs),
    /// Build a plugin tarball from a package directory and print its sha256.
    Package(PluginPackageArgs),
    /// Print a single-use login URL for the plugin's browser routes (valid 60 s).
    Open(PluginOpenArgs),
}

#[derive(Debug, Args)]
pub struct PluginInstallArgs {
    /// Package directory or tarball path (https:// URLs are declarable but
    /// rejected until a TLS-enabled build).
    pub source: String,
    /// Expected sha256 of the tarball.
    #[arg(long)]
    pub sha256: Option<String>,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PluginOpenArgs {
    pub name: String,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ApiOnlyArgs {
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PluginRemoveArgs {
    pub name: String,
    /// Also delete the plugin's state directory and installed packages (needs a running daemon).
    #[arg(long)]
    pub purge: bool,
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PluginPackageArgs {
    /// Package directory holding balerix-plugin.yaml and mise.toml.
    pub dir: PathBuf,
    /// Output tarball (default: ./<name>-<version>.tar.gz).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to bind (default: config.toml `[server] bind`, else 127.0.0.1:7643).
    #[arg(long)]
    pub bind: Option<String>,
    /// Re-exec detached; log to server/server.log; print the endpoint.
    #[arg(short = 'd', long)]
    pub detach: bool,
    /// tmux server socket name (tests use a private one).
    #[arg(long, hide = true, default_value = "balerix")]
    pub tmux_socket: String,
    /// Set by `-d` on the child it spawns.
    #[arg(long, hide = true)]
    pub detached_child: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the fully-resolved spec for a fleet file.
    Resolve(ResolveArgs),
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Print JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum DevCommand {
    /// Render one agent's generated files (settings.json, mise.toml,
    /// nono-profile.json, launch.sh) without launching anything.
    Materialize(MaterializeArgs),
    /// Stand-in for `claude` in the e2e: runs the SessionStart command hooks,
    /// posts Notification, PreToolUse and Stop to their HTTP hooks, records
    /// the replies, echoes stdin to $HOME/fake-claude.stdin, then sleeps.
    FakeClaude(FakeClaudeArgs),
    /// Stand-in plugin for the e2e on the SDK: blocks `rm -rf`, answers Stop
    /// with send_text, observes into scratch.
    FakePlugin,
}

#[derive(Debug, Args)]
pub struct FakeClaudeArgs {
    /// Whatever the fleet passes to claude (`--verbose`, `--continue`, …); recorded, not interpreted.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

#[derive(Debug, Args)]
pub struct MaterializeArgs {
    /// Path to the fleet YAML file.
    pub file: PathBuf,
    /// Agent to render, as crew/agent.
    pub agent: String,
    /// Output root (default: a fresh temp dir, printed).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Fleet name; overrides `name` in the file.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not layer the host's ~/.claude/settings.json beneath the file.
    #[arg(long)]
    pub no_host_defaults: bool,
    /// Where the generated hooks post to.
    #[arg(long, default_value = "http://127.0.0.1:7643")]
    pub hooks_url: String,
    /// Also run `mise install` and `nono profile validate`.
    #[arg(long)]
    pub install: bool,
    /// Write real credentials instead of "<redacted>" placeholders.
    #[arg(long)]
    pub with_credentials: bool,
}
