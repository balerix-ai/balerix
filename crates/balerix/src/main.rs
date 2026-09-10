//! `balerix` — CLI entry point.

mod cli;
mod client;
mod commands;
#[cfg(test)]
mod testutil;
mod wiring;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, ConfigCommand, DevCommand, PluginCommand};

fn main() -> ExitCode {
    match run() {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => commands::serve::serve_command(&args),
        Command::Config {
            command: ConfigCommand::Resolve(args),
        } => commands::config::resolve_command(&args),
        Command::Dev {
            command: DevCommand::Materialize(args),
        } => commands::dev::materialize_command(&args),
        Command::Dev {
            command: DevCommand::FakeClaude(args),
        } => commands::dev::fake_claude_command(&args),
        Command::Dev {
            command: DevCommand::FakePlugin,
        } => commands::dev::fake_plugin_command(),
        Command::Up(args) => commands::fleet::up_command(&args),
        Command::Update(args) => commands::fleet::update_command(&args),
        Command::Down(args) => commands::fleet::down_command(&args),
        Command::Status(args) => commands::fleet::status_command(&args),
        Command::List(args) => commands::fleet::list_command(&args),
        Command::Plugin {
            command: PluginCommand::Install(args),
        } => commands::plugin::install_command(&args),
        Command::Plugin {
            command: PluginCommand::Sync(args),
        } => commands::plugin::sync_command(&args),
        Command::Plugin {
            command: PluginCommand::List(args),
        } => commands::plugin::list_command(&args),
        Command::Plugin {
            command: PluginCommand::Remove(args),
        } => commands::plugin::remove_command(&args),
        Command::Plugin {
            command: PluginCommand::Package(args),
        } => commands::plugin::package_command(&args),
        Command::Plugin {
            command: PluginCommand::Open(args),
        } => commands::plugin::open_command(&args),
        Command::HookRelay => commands::relay::hook_relay_command(),
    }
}
