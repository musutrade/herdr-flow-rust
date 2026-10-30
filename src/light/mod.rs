//! Optional, process-local records. No runtime, dispatch, or legacy state access.
mod error;
mod model;
mod paths;
mod query;
mod schema;
mod store;
mod validate;
mod write;

pub use error::{Error, Result};
pub use model::*;
pub use store::{Options, Store};

#[cfg(test)]
#[path = "../../tests/common/light/mod.rs"]
mod tests;
