//! Turns a three-level fleet YAML file into a fully-resolved `FleetSpec`
//! (spec §5). Pure apart from reading the file and host defaults.

pub mod error;
pub mod file;
pub mod host;
pub mod merge;
pub mod resolve;
pub mod validate;

pub use balerix_core::is_exact_version;
pub use error::ConfigError;
pub use file::{CrewFile, FleetFile, from_value, parse, read};
pub use host::{HostDefaults, HostPaths};
pub use merge::{merge, merge_layers, strip_nulls};
pub use resolve::{ResolveOptions, resolve};
pub use validate::{RESERVED_ENV_PREFIXES, tools_layer, validate_agent};
