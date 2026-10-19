//! herdr-issue-flow as a pure Rust library and command suite.
//!
//! The durable documents on disk (queue state, guard state, CI observer state)
//! intentionally keep the JSON shapes of the original implementation so
//! existing integrations keep working.

#![allow(clippy::too_many_arguments)]

pub mod batch;
pub mod ci;
pub mod codex;
pub mod crap;
pub mod error;
pub mod guard;
pub mod herdr;
pub mod logging;
pub mod metrics;
pub mod model;
pub mod opencode;
pub mod pr;
pub mod progress;
pub mod queue;
pub mod runtime;
pub mod util;
pub mod watch_service;

pub use error::{ErrorKind, FlowError, Result};
