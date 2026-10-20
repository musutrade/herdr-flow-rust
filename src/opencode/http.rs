//! Bounded HTTP transport for an explicitly registered loopback server.
use std::time::Duration;

use base64::Engine;
use serde_json::Value;

use crate::error::{FlowError, Result};
use crate::model::{self, J};

pub trait Api {
    fn get(&self, path: &str) -> Result<J>;
    fn post(&self, path: &str, body: &J) -> Result<J>;
}

pub struct HttpApi {
    endpoint: String,
    cwd: String,
    authorization: Option<String>,
    agent: ureq::Agent,
}

pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    let uri: ureq::http::Uri = endpoint
        .parse()
        .map_err(|_| FlowError::guard("invalid OpenCode endpoint"))?;
    let host_ok = matches!(uri.host(), Some("127.0.0.1" | "[::1]" | "::1"));
    let valid = [
        uri.scheme_str() == Some("http"),
        host_ok,
        uri.port_u16().is_some_and(|port| port > 0),
        uri.path() == "/",
        uri.query().is_none(),
        !uri.authority()
            .is_some_and(|authority| authority.as_str().contains('@')),
    ];
    if !valid.into_iter().all(|ok| ok) {
        return Err(FlowError::guard("OpenCode endpoint must be an explicit loopback HTTP host and port without a path or query"));
    }
    Ok(())
}

impl HttpApi {
    pub fn from_state(state: &J) -> Result<Self> {
        let endpoint = required(state, "endpoint")?;
        validate_endpoint(&endpoint)?;
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            cwd: required(state, "cwd")?,
            authorization: authorization(),
            agent: config.into(),
        })
    }

    fn decode(&self, mut response: ureq::http::Response<ureq::Body>) -> Result<J> {
        if !response.status().is_success() {
            return Err(FlowError::guard(format!(
                "OpenCode HTTP request rejected ({})",
                response.status().as_u16()
            )));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_string()
            .map_err(transport_error)?;
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|_| FlowError::guard("OpenCode returned invalid JSON"))
    }
}

fn authorization() -> Option<String> {
    let password = std::env::var("OPENCODE_SERVER_PASSWORD").ok()?;
    let username = std::env::var("OPENCODE_SERVER_USERNAME").unwrap_or_else(|_| "opencode".into());
    Some(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    ))
}

fn transport_error(error: ureq::Error) -> FlowError {
    // Avoid serializing headers, credentials or response bodies in diagnostics.
    FlowError::guard(format!("OpenCode transport failed: {error}"))
}

pub(super) fn required(state: &J, key: &str) -> Result<String> {
    model::string(state, key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| FlowError::guard(format!("OpenCode registration requires {key}")))
}

impl Api for HttpApi {
    fn get(&self, path: &str) -> Result<J> {
        let mut request = self
            .agent
            .get(format!("{}{path}", self.endpoint))
            .query("directory", &self.cwd);
        if let Some(auth) = &self.authorization {
            request = request.header("Authorization", auth);
        }
        self.decode(request.call().map_err(transport_error)?)
    }

    fn post(&self, path: &str, body: &J) -> Result<J> {
        let mut request = self
            .agent
            .post(format!("{}{path}", self.endpoint))
            .query("directory", &self.cwd);
        if let Some(auth) = &self.authorization {
            request = request.header("Authorization", auth);
        }
        self.decode(request.send_json(body).map_err(transport_error)?)
    }
}
