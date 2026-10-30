//! Optional records and bounded observation; no dispatch or legacy state access.
pub mod cli;
mod cli_args;
mod cli_input;
mod cli_output;
mod cli_run;
mod error;
mod model;
mod observe;
mod observe_cache;
mod observe_notify;
mod observe_source;
mod observe_state;
mod observe_wire;
mod paths;
mod query;
mod schema;
mod store;
mod tui;
mod tui_data;
mod tui_view;
mod validate;
mod write;

pub use error::{Error, Result};
pub use model::*;
pub use store::{Options, Store};

#[cfg(test)]
#[path = "../../tests/common/light/mod.rs"]
mod tests;
