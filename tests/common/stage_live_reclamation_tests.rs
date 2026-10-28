use super::*;

fn fixture() -> (Fixture, task_env::TaskEnvironment, J, J, J) {
    let f = Fixture::new();
    let env = setup(&f, &f.temp.path().join("output.git"));
    let owner =
        serde_json::to_value(lifecycle::task_owner(&f.state(), "task").unwrap().unwrap()).unwrap();
    let entries: Vec<_> = ["dev-a1", "stage-review-a1"].into_iter().map(|assignment| json!({
        "binding":{"assignment":assignment,"lineage":{"scope_epoch":1}},"phase":"complete",
        "runtime":{"lifecycle_owner":owner,"pid":123,"start_time":99},
        "broker":{"pid":123,"start_time":99},"frontends":[{"pid":124,"start_time":100},{"pid":125,"start_time":101}],
        "presentation":{"phase":"closed"},"sessions":[{"phase":"deleted","session":SESSION}]
    })).collect();
    let plan = json!({"phase":"complete","owner":owner,"delivery":"stage-intent","scope_epoch":1,"entries":entries,"retained":[]});
    let parent = json!({"stage_delivery":{"delivery":{"id":"stage-intent"},"resource_cleanup":{
        "phase":"complete","delivery":"stage-intent","selected":2,"completed":2,"retained":[],"error":null}}});
    (f, env, owner, plan, parent)
}

#[test]
fn complete_plan_is_read_from_protected_storage_without_observing_pending() {
    let (f, env, owner, plan, parent) = fixture();
    let root = task_runtime::prepare(&env, "audit-plan-fixture").unwrap();
    util::atomic_write_json(&root.join("env.json"), &json!({"stage_resources":plan})).unwrap();
    assert!(root.join("env.json").is_dir());
    let persisted = read(&root, "env.json").unwrap();
    assert_eq!(completed_plan(&parent, &persisted, &owner), plan);
    for bad in ["pending", "sessions", "retiring"] {
        let mut mutated = persisted.clone();
        mutated["stage_resources"]["entries"][0]["phase"] = json!(bad);
        assert!(std::panic::catch_unwind(|| completed_plan(&parent, &mutated, &owner)).is_err());
    }
    let mut terminal = parent.clone();
    terminal["final_outcome"] = json!("success");
    assert!(std::panic::catch_unwind(|| completed_plan(&terminal, &persisted, &owner)).is_err());
    let mut replaced = persisted.clone();
    replaced["stage_resources"]["owner"]["epoch"] = json!(999);
    assert!(std::panic::catch_unwind(|| completed_plan(&parent, &replaced, &owner)).is_err());
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
}

#[test]
fn only_exact_authoritative_thread_absence_proves_upstream_deletion() {
    let missing = || {
        herdr_flow::FlowError::rpc(format!("thread not found: {SESSION}"))
            .with_rpc(Some(-32600), None)
    };
    assert_eq!(deleted(Err(missing()), SESSION)["session"], SESSION);
    for reply in [
        Ok(json!({"thread":{"id":SESSION,"status":{"type":"notLoaded"}}})),
        Err(herdr_flow::FlowError::rpc_timeout("timeout")),
        Err(herdr_flow::FlowError::rpc("thread not found: other").with_rpc(Some(-32600), None)),
        Err(
            herdr_flow::FlowError::rpc(format!("thread not found: {SESSION}"))
                .with_rpc(Some(-32601), None),
        ),
    ] {
        assert!(std::panic::catch_unwind(|| deleted(reply, SESSION)).is_err());
    }
}

#[test]
fn process_observation_requires_original_start_identity_and_actual_exit() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    let start = task_env::process_start_time(pid).unwrap();
    let identity = json!({"pid":pid,"start_time":start});
    assert!(process_alive(&identity));
    assert!(!process_alive(&json!({"pid":pid,"start_time":start + 1})));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!process_alive(&identity));
}

#[test]
fn visible_identity_matching_does_not_confuse_neighbor_pane_numbers() {
    let panes = json!({"panes":[{"id":"w2:p10","terminal":"unrelated-terminal"}]});
    assert!(!contains_identity(&panes, &json!("w2:p1")));
    assert!(contains_identity(&panes, &json!("w2:p10")));
}
