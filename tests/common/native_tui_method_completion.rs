use super::*;
use crate::task_runtime::tui::method_fixtures as cases;

#[test]
fn observed_native_165_metadata_and_final_command_replies_discharge_only_their_work() {
    let mut fixture = Fixture::new();
    fixture.ready();
    for (id, case) in cases::metadata("session").into_iter().enumerate() {
        fixture.send(
            true,
            json!({"id":id,"method":case.method,"params":case.params}),
        );
        fixture.pending();
        fixture.send(false, json!({"id":id,"result":case.result}));
        fixture.empty();
    }
    let case = cases::command();
    fixture.send(
        true,
        json!({"id":"exec","method":case.method,"params":case.params}),
    );
    fixture.send(false, json!({"method":"command/exec/outputDelta","params":{"processId":"catalog-command","stream":"stdout","deltaBase64":"b2sK"}}));
    fixture.pending();
    fixture.send(false, json!({"id":"exec","result":case.result}));
    fixture.empty();
    for code in [-1, 1, i32::MIN, i32::MAX] {
        fixture.send(true, json!({"id":"exec","method":"command/exec"}));
        fixture.send(false, json!({"id":"exec","result":{"exitCode":code,"stdout":"partial output","stderr":"failed command"}}));
        fixture.empty();
    }
}

#[test]
fn command_acks_malformed_or_out_of_range_results_never_prove_exit() {
    for result in [
        json!({}),
        json!(null),
        json!([]),
        json!({"exitCode":0}),
        json!({"exitCode":0,"stdout":""}),
        json!({"exitCode":0,"stderr":""}),
        json!({"exitCode":"0","stdout":"","stderr":""}),
        json!({"exitCode":0.0,"stdout":"","stderr":""}),
        json!({"exitCode":2147483648_i64,"stdout":"","stderr":""}),
        json!({"exitCode":-2147483649_i64,"stdout":"","stderr":""}),
        json!({"exitCode":0,"stdout":null,"stderr":""}),
        json!({"exitCode":0,"stdout":"","stderr":false}),
    ] {
        let mut fixture = Fixture::new();
        fixture.ready();
        fixture.send(true, json!({"id":1,"method":"command/exec"}));
        fixture.send(false, json!({"id":1,"result":result}));
        fixture.pending();
        fixture.send(false, json!({"id":1,"result":cases::command().result}));
        fixture.send(false, json!({"id":1,"error":{"message":"late duplicate"}}));
        fixture.complete("unrelated");
        fixture.pending();
        let ledger = load(&fixture.binding).unwrap().unwrap();
        assert_eq!(ledger.pending.len(), 1);
        assert!(ledger.pending[0].responded && ledger.pending[0].turn.is_none());
    }
}

#[test]
fn metadata_names_do_not_generalize_to_async_suffixes_or_similar_command_methods() {
    for method in [
        "future/work/list",
        "future/work/read",
        "command/exec/start",
        "command/exec/write",
        "command/exec/terminate",
        "thread/compact/start",
        "thread/shellCommand",
    ] {
        let mut fixture = Fixture::new();
        fixture.ready();
        fixture.send(true, json!({"id":1,"method":method}));
        fixture.send(false, json!({"id":1,"result":cases::command().result}));
        fixture.pending();
    }
}

#[test]
fn persisted_responded_only_debt_is_not_migrated_or_released_by_new_connections() {
    let mut fixture = Fixture::new();
    fixture.ready();
    let mut ledger = load(&fixture.binding).unwrap().unwrap();
    let mut methods: Vec<_> = cases::metadata("session")
        .into_iter()
        .map(|case| case.method)
        .collect();
    methods.push("command/exec");
    ledger.pending = methods
        .into_iter()
        .enumerate()
        .map(|(id, method)| Pending {
            connection: fixture.drain.connection.clone(),
            client: true,
            id: json!(id),
            method: method.into(),
            turn: None,
            responded: true,
        })
        .collect();
    save(&fixture.binding, &ledger).unwrap();
    let before = util::read_state_json(fixture._directory.path(), FILE).unwrap();
    fixture.drain = Drain::new(fixture.binding.clone());
    fixture.ready();
    assert_eq!(
        util::read_state_json(fixture._directory.path(), FILE).unwrap(),
        before
    );
    fixture.send(false, json!({"id":7,"result":cases::command().result}));
    fixture.complete("unrelated");
    fixture.pending();
    assert_eq!(load(&fixture.binding).unwrap().unwrap().pending.len(), 8);
}

#[test]
fn final_command_reply_keeps_typed_id_connection_and_human_request_isolation() {
    let mut fixture = Fixture::new();
    fixture.ready();
    fixture.send(true, json!({"id":7,"method":"command/exec"}));
    fixture.send(true, json!({"id":"7","method":"hooks/list"}));
    fixture.send(false, json!({"id":"7","result":{"data":[]}}));
    fixture.pending();
    fixture.send(false, json!({"id":7,"method":"item/tool/requestUserInput","params":{"threadId":"session","turnId":"human"}}));
    fixture.send(true, json!({"id":7,"result":{"answers":{}}}));
    fixture.send(false, json!({"id":7,"result":cases::command().result}));
    fixture.pending();
    fixture.complete("human");
    fixture.empty();
    fixture.send(true, json!({"id":8,"method":"command/exec"}));
    fixture.drain.connected();
    fixture.send(false, json!({"id":8,"result":cases::command().result}));
    fixture.pending();
}
