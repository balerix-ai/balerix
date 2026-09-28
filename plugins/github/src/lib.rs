//! The GitHub plugin (Spec M): a mention of the App on an issue or pull
//! request starts an agent whose session is that issue. Turns and
//! questions post as comments; permitted comments come back as prompts
//! or answers; a submitted review is one message; the fleet is the
//! repository's own `.balerix.yaml`.

pub mod config;
pub mod github;
pub mod mention;
pub mod prompt;
pub mod repo_config;
pub mod status;
