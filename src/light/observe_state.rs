//! Observations are evidence about a binding, never task outcomes.
use super::Binding;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub binding: Binding,
    pub observed_state: String,
    pub source: String,
    pub observed_at_ms: u64,
    pub stale_after_ms: u64,
    pub reason: String,
    pub service_instance: Option<String>,
    pub server_version: Option<String>,
    pub observer_client_version: String,
    pub state_change_seq: Option<u64>,
    pub completion_seq: Option<u64>,
    pub completion_transition: bool,
    pub continuity: String,
    pub observer: String,
    pub observer_process: Option<(u32, String)>,
}
impl Snapshot {
    pub fn unknown(binding: Binding, now: u64, ttl: u64, reason: &str) -> Self {
        Self {
            binding,
            observed_state: "unknown".into(),
            source: "herdr_json".into(),
            observed_at_ms: now,
            stale_after_ms: ttl,
            reason: reason.into(),
            service_instance: None,
            server_version: None,
            observer_client_version: env!("CARGO_PKG_VERSION").into(),
            state_change_seq: None,
            completion_seq: None,
            completion_transition: false,
            continuity: "baseline".into(),
            observer: "once_complete".into(),
            observer_process: None,
        }
    }
    pub fn current(&mut self, now: u64) {
        let fresh = now
            .checked_sub(self.observed_at_ms)
            .is_some_and(|age| age <= self.stale_after_ms);
        if !fresh {
            self.observed_state = "unknown".into();
            self.reason = "observation_expired_or_clock_changed".into();
            self.completion_transition = false;
            self.continuity = "gap".into();
        }
    }
    pub fn check_observer(&mut self) {
        if self.observer != "watch_lease_unconfirmed" {
            return;
        }
        let alive = self.observer_process.as_ref().is_some_and(|(pid, start)| {
            super::observe_wire::process_start(*pid).ok().as_ref() == Some(start)
        });
        if !alive {
            self.observer = "exited_or_unverifiable".into();
            self.observed_state = "unknown".into();
            self.reason = "observer_exited_or_unverifiable".into();
            self.completion_transition = false;
        }
    }
    pub fn reconcile(&mut self, previous: Option<&Self>, uninterrupted: bool) {
        let Some(prior) = previous.filter(|p| uninterrupted && same_context(p, self)) else {
            return;
        };
        if prior.state_change_seq.zip(prior.completion_seq).is_none()
            || self.state_change_seq.zip(self.completion_seq).is_none()
        {
            self.continuity = "sequences_unavailable".into();
            return;
        }
        self.continuity = "continuous".into();
        self.completion_transition = prior.state_change_seq.zip(self.state_change_seq).is_some()
            && matches!((prior.completion_seq,self.completion_seq),(Some(a),Some(b)) if b>a);
        if decreasing(prior.state_change_seq, self.state_change_seq)
            || decreasing(prior.completion_seq, self.completion_seq)
        {
            self.continuity = "sequence_reset".into();
            self.completion_transition = false;
        }
    }
}
fn same_context(a: &Snapshot, b: &Snapshot) -> bool {
    b.observed_at_ms
        .checked_sub(a.observed_at_ms)
        .is_some_and(|age| age <= a.stale_after_ms)
        && a.binding == b.binding
        && a.service_instance.is_some()
        && a.service_instance == b.service_instance
        && a.observed_state != "unknown"
        && b.observed_state != "unknown"
}
fn decreasing(a: Option<u64>, b: Option<u64>) -> bool {
    matches!((a,b),(Some(a),Some(b)) if b<a)
}
pub(super) fn populate(snapshot: &mut Snapshot, agent: &Value) {
    if !matches_binding(&snapshot.binding, agent) {
        snapshot.reason = "binding_context_missing_or_changed".into();
        return;
    }
    let state = match agent["agent_status"].as_str() {
        Some("working") => "working",
        Some("idle" | "done") => "idle",
        Some("blocked") => "blocked",
        _ => "unknown",
    };
    snapshot.observed_state = state.into();
    snapshot.reason = if state == "unknown" {
        "unsupported_or_missing_status"
    } else {
        "current_agent_state_not_task_outcome"
    }
    .into();
    snapshot.state_change_seq = agent["state_change_seq"].as_u64();
    snapshot.completion_seq = agent["completion_seq"].as_u64();
}
fn matches_binding(binding: &Binding, agent: &Value) -> bool {
    let location = &binding.details.herdr_location;
    let Some(session) = &binding.details.session else {
        return false;
    };
    let Ok(actual) = serde_json::from_value::<super::Session>(agent["agent_session"].clone())
    else {
        return false;
    };
    actual == *session
        && agent["workspace_id"].as_str() == Some(&location.workspace)
        && agent["pane_id"].as_str() == Some(&location.pane)
        && agent["agent"].as_str() == Some(&session.agent)
}
