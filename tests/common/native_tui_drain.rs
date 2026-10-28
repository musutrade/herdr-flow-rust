use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    binding: J,
    drain: Drain,
}

impl Fixture {
    fn new() -> Self {
        let directory = util::fs::test_tempdir();
        let binding = json!({"runtime":directory.path(),"directory":directory.path(),
            "instance":"owner","nonce":"attachment","session":"session"});
        let drain = Drain::new(binding.clone());
        Self {
            _directory: directory,
            binding,
            drain,
        }
    }
    fn ready(&mut self) {
        self.drain.initialize().unwrap();
    }
    fn pending(&self) {
        assert!(require_drained(&self.binding).is_err());
    }
    fn empty(&self) {
        require_drained(&self.binding).unwrap();
    }
    fn send(&mut self, client: bool, message: J) {
        self.drain.message(client, &message).unwrap();
    }
    fn complete(&mut self, turn: &str) {
        self.send(
            false,
            json!({"method":"turn/completed","params":{
            "threadId":"session","turn":{"id":turn,"status":"completed"}}}),
        );
    }
}

#[test]
fn missing_replaced_or_disappeared_evidence_never_means_drained() {
    let mut f = Fixture::new();
    f.pending();
    f.ready();
    f.empty();
    let mut replacement = f.binding.clone();
    replacement["nonce"] = json!("replacement");
    assert!(require_drained(&replacement).is_err());
    std::fs::remove_file(f._directory.path().join(FILE)).unwrap();
    assert!(f.drain.initialize().is_err());
    f.pending();
    util::atomic_write_json(
        &f._directory.path().join("stage-retirement.json"),
        &json!({}),
    )
    .unwrap();
    let mut restarted = Drain::new(f.binding.clone());
    assert!(restarted.initialize().is_err());
    f.pending();
}

#[test]
fn start_acknowledgement_is_not_turn_completion_and_typed_ids_do_not_collide() {
    let mut f = Fixture::new();
    f.ready();
    f.send(true, json!({"id":1,"method":"turn/start"}));
    f.send(false, json!({"id":"1","result":{"turn":{"id":"wrong"}}}));
    f.pending();
    f.send(false, json!({"id":1,"result":{"turn":{"id":"active"}}}));
    f.pending();
    // Reusing an already answered request ID does not release its active turn.
    f.send(true, json!({"id":1,"method":"thread/read"}));
    f.send(false, json!({"id":1,"result":{}}));
    f.pending();
    f.complete("wrong");
    f.pending();
    f.complete("active");
    f.empty();
}

#[test]
fn terminal_event_before_response_reconciles_only_its_original_connection() {
    let mut f = Fixture::new();
    f.ready();
    f.send(true, json!({"id":2,"method":"turn/start"}));
    f.complete("fast");
    f.pending();
    f.send(false, json!({"id":2,"result":{"turn":{"id":"fast"}}}));
    f.empty();
    f.send(true, json!({"id":3,"method":"turn/start"}));
    f.drain.connected();
    f.send(
        false,
        json!({"id":3,"error":{"message":"foreign connection"}}),
    );
    f.complete("old");
    f.pending();
    f.drain = Drain::new(f.binding.clone());
    f.ready();
    f.send(false, json!({"id":3,"result":{"turn":{"id":"old"}}}));
    f.pending();
}

#[test]
fn retirement_refuses_new_operations_but_preserves_answers_and_interrupt() {
    let mut f = Fixture::new();
    f.ready();
    f.send(
        false,
        json!({"id":7,"method":"item/tool/requestUserInput","params":{
        "threadId":"session","turnId":"active"}}),
    );
    util::atomic_write_json(
        &f._directory.path().join("stage-retirement.json"),
        &json!({}),
    )
    .unwrap();
    assert!(f
        .drain
        .message(true, &json!({"id":8,"method":"turn/start"}))
        .is_err());
    assert!(f
        .drain
        .message(true, &json!({"method":"turn/start"}))
        .is_err());
    f.send(true, json!({"method":"initialized"}));
    f.send(true, json!({"id":7,"result":{"answers":{}}}));
    f.pending();
    f.send(true, json!({"id":9,"method":"turn/interrupt"}));
    f.send(false, json!({"id":9,"result":{}}));
    f.pending();
    f.send(
        false,
        json!({"method":"turn/completed","params":{
        "threadId":"foreign","turn":{"id":"active","status":"completed"}}}),
    );
    f.pending();
    f.complete("active");
    f.empty();
}

#[test]
fn server_request_with_client_id_cannot_discharge_client_request() {
    let mut f = Fixture::new();
    f.ready();
    f.send(true, json!({"id":1,"method":"turn/start"}));
    f.send(
        false,
        json!({"id":1,"method":"item/commandExecution/requestApproval",
        "params":{"threadId":"session","turnId":"active"}}),
    );
    f.send(true, json!({"id":1,"result":{"decision":"accept"}}));
    f.complete("active");
    f.pending();
    f.send(false, json!({"id":1,"result":{"turn":{"id":"active"}}}));
    f.empty();
}

#[test]
fn malformed_and_unknown_work_results_keep_the_original_obligation() {
    let mut f = Fixture::new();
    f.ready();
    f.send(true, json!({"id":1,"method":"turn/start"}));
    assert!(f
        .drain
        .message(false, &json!({"id":1,"result":{},"error":{}}))
        .is_err());
    f.send(false, json!({"id":1,"result":{}}));
    f.pending();
    f.send(
        false,
        json!({"id":1,"error":{"message":"definite refusal"}}),
    );
    f.empty();
    f.send(true, json!({"id":2,"method":"review/start"}));
    f.send(false, json!({"id":2,"result":{"reviewThreadId":"other"}}));
    f.pending();
}

#[test]
fn ledger_bounds_preserve_existing_work() {
    let mut f = Fixture::new();
    f.ready();
    let mut ledger = load(&f.binding).unwrap().unwrap();
    ledger.pending = (0..CAPACITY)
        .map(|id| Pending {
            connection: f.drain.connection.clone(),
            client: true,
            id: json!(id),
            method: "thread/read".into(),
            turn: None,
            responded: false,
        })
        .collect();
    save(&f.binding, &ledger).unwrap();
    assert!(f
        .drain
        .message(true, &json!({"id":CAPACITY,"method":"thread/read"}))
        .is_err());
    f.pending();
    f.send(false, json!({"id":0,"result":{}}));
    f.send(true, json!({"id":CAPACITY,"method":"thread/read"}));
    assert_eq!(load(&f.binding).unwrap().unwrap().pending.len(), CAPACITY);
    ledger.pending.clear();
    ledger.completed = (0..CAPACITY)
        .map(|id| Completed {
            connection: f.drain.connection.clone(),
            turn: id.to_string(),
        })
        .collect();
    save(&f.binding, &ledger).unwrap();
    f.complete("last");
    f.empty();
    assert_eq!(load(&f.binding).unwrap().unwrap().completed.len(), CAPACITY);
}
