//! `balerix config …`

use anyhow::Result;
use balerix_config::{HostPaths, ResolveOptions, host, read, resolve};

use crate::cli::ResolveArgs;

/// Resolves a fleet file and renders the spec. Credentials discovered on
/// the host are dropped here — this command never prints them.
pub fn resolve_command(args: &ResolveArgs) -> Result<String> {
    let file = read(&args.file)?;
    let host_claude_settings = if args.no_host_defaults {
        None
    } else {
        host::load(&HostPaths::discover()?)?.claude_settings
    };
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: args.name.clone(),
            host_claude_settings,
        },
    )?;
    Ok(if args.json {
        serde_json::to_string_pretty(&spec)? + "\n"
    } else {
        serde_norway::to_string(&spec)?
    })
}
