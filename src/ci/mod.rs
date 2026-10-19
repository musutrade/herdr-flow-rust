//! Read-only GitHub Actions and external Check observation.

pub mod checks;
pub mod policy;
pub mod watch;

pub use checks::validate as validate_checks;
pub use policy::required_jobs;
pub use watch::{
    background, config_hash, observe, persist, result_code, run, safe_json_load, validate, GitHub,
    GitHubApi,
};
