//! Durable work admitted by one native attachment. A lifecycle SH protects
//! admission/observation; stage retirement holds EX while checking this ledger.
//! Bytes may still be buffered after admission, so a socket write is not proof
//! of completion. Unknown outcomes survive disconnects and are never replayed.
use super::*;
use serde::{Deserialize, Serialize};

const FILE: &str = "native-requests.json";
const LIMIT: usize = 1024 * 1024;
const CAPACITY: usize = 256;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u8,
    attachment: J,
    pending: Vec<Pending>,
    completed: Vec<Completed>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    connection: String,
    client: bool,
    id: J,
    method: String,
    turn: Option<String>,
    responded: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completed {
    connection: String,
    turn: String,
}

fn identity(binding: &J) -> J {
    json!({"runtime":binding["runtime"],"instance":binding["instance"],
        "session":binding["session"],"nonce":binding["nonce"],"directory":binding["directory"]})
}

fn load(binding: &J) -> Result<Option<Ledger>> {
    let directory = PathBuf::from(required(binding, "directory")?);
    let Some(value) = util::read_state_json_bounded_if_exists(&directory, FILE, LIMIT)? else {
        return Ok(None);
    };
    let ledger: Ledger = serde_json::from_value(value)?;
    ensure(
        ledger.version == 1 && ledger.attachment == identity(binding),
        "native TUI drain identity changed",
    )?;
    ensure(
        ledger.pending.len() <= CAPACITY && ledger.completed.len() <= CAPACITY,
        "native TUI drain ledger exceeds bound",
    )?;
    Ok(Some(ledger))
}

fn save(binding: &J, ledger: &Ledger) -> Result<()> {
    let value = serde_json::to_value(ledger)?;
    ensure(
        serde_json::to_vec(&value)?.len() <= LIMIT,
        "native TUI drain ledger exceeds bound",
    )?;
    util::atomic_write_json(
        &PathBuf::from(required(binding, "directory")?).join(FILE),
        &value,
    )
}

/// Called under the parent's EX, before closing the original presentation.
/// Missing evidence from an older bridge cannot establish safe retirement.
pub(super) fn require_drained(binding: &J) -> Result<()> {
    let ledger = load(binding)?.ok_or_else(|| {
        FlowError::guard("native TUI drain evidence missing; retain presentation")
    })?;
    ensure(
        ledger.pending.is_empty(),
        "native TUI has unconfirmed requests or turns; retain approval/input channel",
    )
}

pub(super) struct Drain {
    binding: J,
    connection: String,
    initialized: bool,
}

impl Drain {
    pub fn new(binding: J) -> Self {
        Self {
            binding,
            connection: util::pyjson::uuid_hex(),
            initialized: false,
        }
    }

    pub fn connected(&mut self) {
        // A new connection must never discharge an old connection's requests.
        self.connection = util::pyjson::uuid_hex();
    }

    pub fn root(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(required(&self.binding, "runtime")?))
    }

    pub fn initialize(&mut self) -> Result<()> {
        if load(&self.binding)?.is_none() {
            ensure(!self.initialized, "native TUI drain evidence disappeared")?;
            super::super::retirement::admit(&self.root()?)?;
            save(
                &self.binding,
                &Ledger {
                    version: 1,
                    attachment: identity(&self.binding),
                    pending: vec![],
                    completed: vec![],
                },
            )?;
        }
        self.initialized = true;
        Ok(())
    }

    pub fn message(&mut self, client: bool, message: &J) -> Result<()> {
        if !client && untracked_notification(message) {
            return Ok(());
        }
        let mut ledger = load(&self.binding)?
            .ok_or_else(|| FlowError::guard("native TUI drain evidence disappeared"))?;
        if let Some(method) = message["method"].as_str() {
            self.method(&mut ledger, client, method, message)?;
        } else {
            self.response(&mut ledger, client, message)?;
        }
        save(&self.binding, &ledger)
    }

    fn method(&self, ledger: &mut Ledger, client: bool, method: &str, message: &J) -> Result<()> {
        if client {
            if method == "initialized" && message.get("id").is_none() {
                return Ok(());
            }
            ensure(
                valid_id(&message["id"]),
                "native operation requires an RPC request ID",
            )?;
            if method != "turn/interrupt" {
                super::super::retirement::admit(&self.root()?)?;
            }
        } else if method == "turn/completed" {
            self.completed(ledger, message);
            return Ok(());
        } else if !valid_id(&message["id"]) {
            return Ok(());
        }
        self.reserve(ledger, client, method, message)
    }

    fn reserve(&self, ledger: &mut Ledger, client: bool, method: &str, message: &J) -> Result<()> {
        ensure(
            ledger.pending.len() < CAPACITY,
            "too many unconfirmed native requests",
        )?;
        ensure(
            !ledger
                .pending
                .iter()
                .any(|pending| self.matches(pending, client, message)),
            "duplicate pending native request ID",
        )?;
        let turn = if !client && message["params"]["threadId"] == self.binding["session"] {
            turn_id(&message["params"]["turnId"])
        } else {
            None
        };
        ledger.pending.push(Pending {
            connection: self.connection.clone(),
            client,
            id: message["id"].clone(),
            method: method.into(),
            turn,
            responded: false,
        });
        Ok(())
    }

    fn matches(&self, pending: &Pending, client: bool, message: &J) -> bool {
        !pending.responded
            && pending.connection == self.connection
            && pending.client == client
            && pending.id == message["id"]
    }

    fn response(&self, ledger: &mut Ledger, client: bool, message: &J) -> Result<()> {
        let Some(index) = ledger
            .pending
            .iter()
            .position(|p| self.matches(p, !client, message))
        else {
            return Ok(());
        };
        // A server request with a colliding ID was handled by `method`, not here.
        ensure(
            message.get("result").is_some() != message.get("error").is_some(),
            "native response outcome is ambiguous",
        )?;
        if client {
            // An answer can remain in the bridge or kernel buffer. Keep the
            // interaction until the server proves that its turn has ended.
            ledger.pending[index].responded = true;
        } else if message.get("error").is_some() {
            ledger.pending.remove(index);
        } else {
            self.success(ledger, index, &message["result"]);
        }
        Ok(())
    }

    fn success(&self, ledger: &mut Ledger, index: usize, result: &J) {
        let pending = &mut ledger.pending[index];
        // An acknowledgment consumes this RPC ID even when it cannot prove
        // the work complete. A later reused ID/error must not erase that debt.
        pending.responded = true;
        let turn = match pending.method.as_str() {
            "turn/start" => turn_id(&result["turn"]["id"]),
            "turn/steer" => turn_id(&result["turnId"]),
            method if response_completes(method) => {
                ledger.pending.remove(index);
                return;
            }
            // Compact, shell commands and other operations can acknowledge
            // queueing before native activity is visible. No inferred turn ID.
            _ => None,
        };
        let Some(turn) = turn else {
            return;
        };
        pending.turn = Some(turn.clone());
        if ledger
            .completed
            .iter()
            .any(|c| c.connection == self.connection && c.turn == turn)
        {
            ledger.pending.remove(index);
        }
    }

    fn completed(&self, ledger: &mut Ledger, message: &J) {
        let params = &message["params"];
        if params["threadId"] != self.binding["session"] || !terminal(&params["turn"]["status"]) {
            return;
        }
        let Some(turn) = turn_id(&params["turn"]["id"]) else {
            return;
        };
        ledger
            .pending
            .retain(|p| p.connection != self.connection || p.turn.as_ref() != Some(&turn));
        // Completion may arrive before the turn/start result. Preserve bounded
        // evidence for that exact connection; eviction can only retain work.
        ledger.completed.push(Completed {
            connection: self.connection.clone(),
            turn,
        });
        if ledger.completed.len() > CAPACITY {
            ledger.completed.remove(0);
        }
    }
}

fn response_completes(method: &str) -> bool {
    // Exact protocol methods only. Unknown reads also retain evidence until
    // their semantics are supported; a suffix/prefix cannot prove no work.
    const METHODS: &[&str] = &[
        "initialize",
        "thread/resume",
        "thread/read",
        "thread/list",
        "thread/loaded/list",
        "thread/turns/list",
        "model/list",
        "skills/list",
        "mcpServerStatus/list",
        "app/list",
        "plugin/list",
        "config/read",
        "configRequirements/read",
        "account/read",
        "account/rateLimits/read",
        "experimentalFeature/list",
        "externalAgentConfig/detect",
        "turn/interrupt",
    ];
    METHODS.contains(&method)
}

fn valid_id(id: &J) -> bool {
    id.is_number() || id.as_str().is_some_and(|value| value.len() <= 1024)
}

fn untracked_notification(message: &J) -> bool {
    message["method"]
        .as_str()
        .is_some_and(|method| method != "turn/completed")
        && !valid_id(&message["id"])
}

fn turn_id(id: &J) -> Option<String> {
    id.as_str()
        .filter(|id| !id.is_empty() && id.len() <= 1024)
        .map(str::to_owned)
}

fn terminal(status: &J) -> bool {
    matches!(
        status.as_str(),
        Some("completed" | "interrupted" | "failed")
    )
}

#[cfg(test)]
#[path = "../../../tests/common/native_tui_drain.rs"]
mod tests;
