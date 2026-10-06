//! Pure: observed objects in, typed objects and conditions out. Nothing
//! random or clock-reading happens here; tokens, certificates and the time
//! come in as inputs made by the controllers (sub-project 3b), which do the
//! reading of the cluster and the writing back.

pub mod agent;
pub mod common;
pub mod daemon;
pub mod fleet;
pub mod jobs;
pub mod names;
pub mod plugin;
