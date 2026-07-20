//! CLI subcommand handlers, grouped by surface area. `main.rs` keeps the clap
//! definitions, the dispatch match, and the small shared helpers (project-root
//! resolution, DB opening, config loading); each module here owns the handler
//! bodies for one command family.

pub(crate) mod features;
pub(crate) mod hooks;
pub(crate) mod index;
pub(crate) mod query;
pub(crate) mod risk;
pub(crate) mod runtime;
pub(crate) mod session;
