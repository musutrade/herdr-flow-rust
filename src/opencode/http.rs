//! Bounded HTTP transport for an explicitly registered loopback server.
use std::io::ErrorKind;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;

use crate::error::{FlowError, Result};
use crate::model::{self, J};

const REQUEST_BUDGET: Duration = Duration::from_secs(10);
const GET_ATTEMPTS: usize = 3;

enum ReadError {
    Transport(ureq::Error),
    Response(FlowError),
}

impl ReadError {
    fn interrupted(&self) -> bool {
        matches!(self, Self::Transport(ureq::Error::Io(error)) if error.kind() == ErrorKind::Interrupted)
    }

    fn into_flow(self) -> FlowError {
        match self {
            Self::Transport(error) => transport_error(error),
            Self::Response(error) => error,
        }
    }
}

// Only read-only observations enter this loop. Each attempt includes the body
// read and receives the remaining budget, never a fresh ten-second timeout.
fn observe_get(
    mut remaining: impl FnMut() -> Duration,
    mut read: impl FnMut(Duration) -> std::result::Result<J, ReadError>,
) -> Result<J> {
    let mut attempts = 0;
    loop {
        let budget = remaining();
        if budget.is_zero() {
            return Err(transport_error(ureq::Error::Timeout(ureq::Timeout::Global)));
        }
        attempts += 1;
        let result = read(budget);
        if attempts < GET_ATTEMPTS && result.as_ref().is_err_and(|error| error.interrupted()) {
            continue;
        }
        return result.map_err(ReadError::into_flow);
    }
}

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
            .timeout_global(Some(REQUEST_BUDGET))
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
        Self::decode_response(&mut response).map_err(ReadError::into_flow)
    }

    fn decode_response(
        response: &mut ureq::http::Response<ureq::Body>,
    ) -> std::result::Result<J, ReadError> {
        if !response.status().is_success() {
            return Err(ReadError::Response(FlowError::guard(format!(
                "OpenCode HTTP request rejected ({})",
                response.status().as_u16()
            ))));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_string()
            .map_err(ReadError::Transport)?;
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body)
            .map_err(|_| ReadError::Response(FlowError::guard("OpenCode returned invalid JSON")))
    }

    fn get_once(&self, path: &str, budget: Duration) -> std::result::Result<J, ReadError> {
        let mut request = self.agent.get(format!("{}{path}", self.endpoint));
        if !path.starts_with("/global/") {
            request = request.query("directory", &self.cwd);
        }
        if let Some(auth) = &self.authorization {
            request = request.header("Authorization", auth);
        }
        let mut response = request
            .config()
            .timeout_global(Some(budget))
            .build()
            .call()
            .map_err(ReadError::Transport)?;
        Self::decode_response(&mut response)
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
        let started = Instant::now();
        observe_get(
            || REQUEST_BUDGET.saturating_sub(started.elapsed()),
            |budget| self.get_once(path, budget),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, Read};
    use std::sync::{Arc, Mutex};
    use ureq::http::{Request, Response};
    use ureq::middleware::MiddlewareNext;

    type Reply = std::result::Result<Response<ureq::Body>, ureq::Error>;

    fn interrupted() -> ureq::Error {
        ureq::Error::Io(io::Error::from(ErrorKind::Interrupted))
    }

    fn response(status: u16, body: &str) -> Reply {
        Ok(Response::builder()
            .status(status)
            .body(ureq::Body::builder().data(body))
            .unwrap())
    }

    // Inject faults at the ureq boundary without real models, signals or sleeps.
    fn fixture(replies: Vec<Reply>) -> (HttpApi, Arc<Mutex<Vec<String>>>) {
        let replies = Mutex::new(VecDeque::from(replies));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let agent = ureq::Agent::config_builder()
            .middleware(move |request: Request<ureq::SendBody>, _: MiddlewareNext| {
                assert_eq!(request.headers()["Authorization"], "Basic fixture");
                captured
                    .lock()
                    .unwrap()
                    .push(format!("{} {}", request.method(), request.uri()));
                replies.lock().unwrap().pop_front().expect("extra request")
            })
            .build()
            .into();
        (
            HttpApi {
                endpoint: "http://127.0.0.1:4097".into(),
                cwd: "/fixture".into(),
                authorization: Some("Basic fixture".into()),
                agent,
            },
            calls,
        )
    }

    #[test]
    fn interrupted_get_repeats_only_the_exact_observation() {
        let (api, calls) = fixture(vec![Err(interrupted()), response(200, "{\"ok\":true}")]);
        assert_eq!(api.get("/session/exact").unwrap()["ok"], true);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(
            calls[0],
            "GET http://127.0.0.1:4097/session/exact?directory=%2Ffixture"
        );
    }

    #[test]
    fn get_exhaustion_returns_interrupted_after_three_attempts() {
        let (api, calls) = fixture((0..GET_ATTEMPTS).map(|_| Err(interrupted())).collect());
        let error = api.get("/global/health").unwrap_err();
        assert_eq!(error.message, transport_error(interrupted()).message);
        assert_eq!(calls.lock().unwrap().len(), GET_ATTEMPTS);
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call == "GET http://127.0.0.1:4097/global/health"));
    }

    #[test]
    fn last_get_attempt_can_succeed_and_other_errors_stop_a_retry() {
        let (api, calls) = fixture(vec![
            Err(interrupted()),
            Err(interrupted()),
            response(200, "{}"),
        ]);
        assert_eq!(api.get("/global/health").unwrap(), serde_json::json!({}));
        assert_eq!(calls.lock().unwrap().len(), GET_ATTEMPTS);
        let (api, calls) = fixture(vec![
            Err(interrupted()),
            Err(ureq::Error::Io(io::Error::from(ErrorKind::ConnectionReset))),
        ]);
        let expected =
            transport_error(ureq::Error::Io(io::Error::from(ErrorKind::ConnectionReset)));
        assert_eq!(
            api.get("/global/health").unwrap_err().message,
            expected.message
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn get_retries_share_budget_and_stop_at_deadline() {
        let mut budgets = [
            Duration::from_secs(10),
            Duration::from_secs(4),
            Duration::ZERO,
        ]
        .into_iter();
        let mut seen = Vec::new();
        let error = observe_get(
            || budgets.next().unwrap(),
            |budget| {
                seen.push(budget);
                Err(ReadError::Transport(interrupted()))
            },
        )
        .unwrap_err();
        assert_eq!(seen, [Duration::from_secs(10), Duration::from_secs(4)]);
        assert_eq!(
            error.message,
            transport_error(ureq::Error::Timeout(ureq::Timeout::Global)).message
        );
        assert!(observe_get(|| Duration::ZERO, |_| panic!("expired attempt")).is_err());
    }

    #[test]
    fn other_transport_errors_and_text_lookalikes_are_not_retried() {
        for error in [
            ureq::Error::Io(io::Error::from(ErrorKind::ConnectionReset)),
            ureq::Error::Io(io::Error::from(ErrorKind::TimedOut)),
            ureq::Error::Io(io::Error::other("Interrupted system call (os error 4)")),
            ureq::Error::Timeout(ureq::Timeout::Global),
        ] {
            let expected = format!("OpenCode transport failed: {error}");
            let (api, calls) = fixture(vec![Err(error)]);
            assert_eq!(api.get("/global/health").unwrap_err().message, expected);
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn rejected_and_invalid_responses_are_not_retried() {
        for (status, body, message) in [
            (503, "", "OpenCode HTTP request rejected (503)"),
            (200, "invalid", "OpenCode returned invalid JSON"),
        ] {
            let (api, calls) = fixture(vec![response(status, body)]);
            assert_eq!(api.get("/global/health").unwrap_err().message, message);
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
        let (api, calls) = fixture(vec![response(204, "")]);
        assert_eq!(api.get("/global/health").unwrap(), Value::Null);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    struct FailedBody {
        wrote_prefix: bool,
    }

    impl Read for FailedBody {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.wrote_prefix {
                buffer[0] = b'[';
                self.wrote_prefix = true;
                return Ok(1);
            }
            // Preserve a typed ureq error across the body reader's io boundary.
            Err(io::Error::other(interrupted()))
        }
    }

    fn interrupted_body() -> Reply {
        Ok(Response::new(ureq::Body::builder().reader(FailedBody {
            wrote_prefix: false,
        })))
    }

    #[test]
    fn interrupted_body_is_retryable_only_for_get() {
        let (api, calls) = fixture(vec![interrupted_body(), response(200, "{}")]);
        assert_eq!(api.get("/global/health").unwrap(), serde_json::json!({}));
        assert_eq!(calls.lock().unwrap().len(), 2);
        for reply in [Err(interrupted()), interrupted_body()] {
            let (api, calls) = fixture(vec![reply]);
            assert_eq!(
                api.post("/session/exact/prompt_async", &Value::Null)
                    .unwrap_err()
                    .message,
                transport_error(interrupted()).message
            );
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(calls[0].starts_with("POST "));
        }
    }
}
