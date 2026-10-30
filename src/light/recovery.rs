//! Restricted recovery kernel. No production sender is enabled by this module.
//!
//! A sender must compare the exact thread AND latest failed turn atomically with
//! accepting the continuation. A separate read followed by turn/start is not one.
use super::{recovery_ledger::Ledger, Error, Options, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use super::recovery_history::read_latest;

pub const CONTINUE_MESSAGE: &str = "The previous turn stopped with a recoverable service error. Check actual progress and existing results first, then continue only the already authorized work. Do not replay tools, messages, CI, push, merge, or cleanup blindly. Resolve unknown side effects before acting; retain the current model, approvals and permissions.";
pub const DELAYS_SECONDS: [i64; 3] = [30, 60, 120];

/// Explicit authorization identity; changing any field does not reset a ledger.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Authorization {
    pub thread_id: String,
    pub project: PathBuf,
    pub history: PathBuf,
    /// Only failures after this activation boundary can start a chain.
    pub not_before: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Turn {
    pub thread_id: String,
    pub turn_id: String,
    pub ordinal: i64,
    pub status: String,
    pub error_code: Option<String>,
    pub completed_at: Option<i64>,
    /// Unknown or unfinished items fail closed. History alone cannot attest to
    /// external side effects or the current live server's quiescence.
    pub items_terminal: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        turn_id: String,
    },
    /// A definitive server rejection is also charged; no same-turn resend.
    Rejected,
    Unknown,
}

/// Implement only against a platform primitive that atomically checks identity,
/// latest failed turn, quiescence, approval and pending-input conditions. The
/// primitive must return the new turn id. Network ambiguity is always Unknown.
pub trait ConstrainedSender {
    fn send(&mut self, authorization: &Authorization, failed: &Turn, message: &str) -> Outcome;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Decision {
    Ineligible,
    Waiting { until: i64 },
    Unknown,
    Exhausted,
    Completed,
    Attempt { number: usize, outcome: Outcome },
}

/// One controller owns a private ledger and nonblocking process lock until drop.
/// A panic, kill or I/O failure after reservation leaves an Unknown attempt.
pub struct Controller {
    ledger: Ledger,
    authorization: Authorization,
}
impl Controller {
    pub fn open(options: &Options, authorization: Authorization) -> Result<Self> {
        validate(&authorization)?;
        let ledger = Ledger::open(options, &authorization)?;
        Ok(Self {
            ledger,
            authorization,
        })
    }

    pub fn step(
        &mut self,
        turn: &Turn,
        now: i64,
        sender: &mut impl ConstrainedSender,
    ) -> Result<Decision> {
        if turn.thread_id != self.authorization.thread_id || turn.turn_id.is_empty() {
            return Ok(Decision::Ineligible);
        }
        let mut state = self.ledger.load()?;
        let decision = state.prepare(turn, now, self.authorization.not_before);
        self.ledger.save(&state)?;
        let Decision::Attempt { number, .. } = decision else {
            return Ok(decision);
        };
        // Reservation is durable before invoking even a faulty sender.
        let outcome = sender.send(&self.authorization, turn, CONTINUE_MESSAGE);
        let outcome = match outcome {
            Outcome::Accepted { ref turn_id } if turn_id.is_empty() || turn_id == &turn.turn_id => {
                Outcome::Unknown
            }
            other => other,
        };
        state.finish(outcome.clone());
        self.ledger.save(&state)?;
        Ok(Decision::Attempt { number, outcome })
    }
}

fn validate(a: &Authorization) -> Result<()> {
    let uuid = a.thread_id.as_bytes();
    let valid = uuid.len() == 36
        && uuid.iter().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if !valid || !a.project.is_absolute() || !a.history.is_absolute() || a.not_before < 0 {
        return Err(Error::Invalid(
            "recovery requires exact thread UUID, absolute project/history and activation time"
                .into(),
        ));
    }
    Ok(())
}

#[derive(Default, Debug, Deserialize, Serialize)]
pub(super) struct State {
    high_water: i64,
    root: Option<String>,
    attempts: Vec<Attempt>,
}
#[derive(Debug, Deserialize, Serialize)]
struct Attempt {
    failed_turn: String,
    outcome: Outcome,
}
impl State {
    fn prepare(&mut self, t: &Turn, now: i64, activated: i64) -> Decision {
        if self.attempts.iter().any(|a| a.outcome == Outcome::Unknown) {
            return Decision::Unknown;
        }
        if t.ordinal < self.high_water {
            return Decision::Ineligible;
        }
        // Even an unrelated/active turn fences older history projections. It
        // must not clear the chain or allow an older failed child to reappear.
        self.high_water = t.ordinal;
        if !self.matches_successor(t) {
            return Decision::Ineligible;
        }
        if t.status == "completed"
            && t.items_terminal
            && t.completed_at.is_some_and(|s| s >= activated && s <= now)
        {
            self.high_water = t.ordinal.saturating_add(1);
            self.root = None;
            self.attempts.clear();
            return Decision::Completed;
        }
        if !eligible(t, now, activated) {
            return Decision::Ineligible;
        }
        self.reserve(t, now)
    }
    fn matches_successor(&self, t: &Turn) -> bool {
        match self.attempts.last().map(|a| &a.outcome) {
            None => true,
            Some(Outcome::Accepted { turn_id }) => turn_id == &t.turn_id,
            _ => false,
        }
    }
    fn reserve(&mut self, t: &Turn, now: i64) -> Decision {
        if self.attempts.len() >= 3 {
            return Decision::Exhausted;
        }
        let until = t
            .completed_at
            .unwrap_or(now)
            .saturating_add(DELAYS_SECONDS[self.attempts.len()]);
        if now < until {
            return Decision::Waiting { until };
        }
        self.root.get_or_insert_with(|| t.turn_id.clone());
        self.high_water = t.ordinal;
        self.attempts.push(Attempt {
            failed_turn: t.turn_id.clone(),
            outcome: Outcome::Unknown,
        });
        Decision::Attempt {
            number: self.attempts.len(),
            outcome: Outcome::Unknown,
        }
    }
    fn finish(&mut self, outcome: Outcome) {
        if let Some(last) = self.attempts.last_mut() {
            last.outcome = outcome;
        }
    }
}

fn eligible(t: &Turn, now: i64, activated: i64) -> bool {
    // No substring/WARN/HTTP-429 classifier. Quota, authentication, policy,
    // context exhaustion, approvals and unrecognized transport errors stay out.
    t.status == "failed"
        && t.items_terminal
        && t.completed_at
            .is_some_and(|s| s >= activated && s <= now && now.saturating_sub(s) <= 600)
        && matches!(
            t.error_code.as_deref(),
            Some(
                "serverOverloaded"
                    | "responseStreamDisconnected"
                    | "responseStreamConnectionFailed"
            )
        )
}
