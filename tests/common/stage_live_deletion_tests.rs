use super::*;

const LIVE: &str = "22222222-2222-2222-2222-222222222222";
fn scope() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/owned/checkout"),
        PathBuf::from("/owned/reviews/stage-review-a1"),
    ]
}
fn expected() -> BTreeMap<String, PathBuf> {
    [(SESSION.to_owned(), scope()[0].clone())]
        .into_iter()
        .collect()
}
fn unloaded(id: &str) -> herdr_flow::FlowError {
    let message = format!("thread not loaded: {id}");
    herdr_flow::FlowError::rpc(&message)
        .with_rpc(Some(-32600), Some(json!({"code":-32600,"message":message})))
        .with_method("thread/read")
}
fn row(id: &str, cwd: &str) -> J {
    json!({"id":id,"cwd":cwd,"cliVersion":"0.165.0","createdAt":1,"updatedAt":1,
        "ephemeral":false,"modelProvider":"openai","preview":"","projectId":null,"sessionId":null,
        "source":"cli","status":{"type":"notLoaded"},"turns":[]})
}
fn with_list(mut list: impl FnMut(&J) -> herdr_flow::Result<J>) -> J {
    prove(
        &scope(),
        &expected(),
        &[LIVE.to_owned()],
        &mut |method, params| {
            if method == "thread/read" {
                return Err(unloaded(params["threadId"].as_str().unwrap()));
            }
            assert_eq!(method, "thread/list");
            assert_eq!(params["cwd"], json!(scope()));
            assert_eq!(params["sourceKinds"], json!(SOURCE_KINDS));
            assert_eq!(params["useStateDbOnly"], true);
            assert_eq!(params["modelProviders"], json!([]));
            assert!(
                params.get("searchTerm").is_none() && params.get("excludedThreadIds").is_none()
            );
            list(params)
        },
    )
}

#[test]
fn actual_rpc_not_loaded_plus_complete_scoped_durable_inventories_proves_absence() {
    let server = crate::rpc_server::Server::start(|mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        assert_eq!(request["params"]["threadId"], SESSION);
        peer.json(&json!({"id":request["id"],"error":{"code":-32600,"message":format!("thread not loaded: {SESSION}")}}));
        for (parent, archived) in [
            (None, false),
            (None, true),
            (Some(LIVE), false),
            (Some(LIVE), true),
        ] {
            let request = peer.request();
            assert_eq!(request["method"], "thread/list");
            assert_eq!(request["params"]["cwd"], json!(scope()));
            assert_eq!(request["params"]["archived"], archived);
            assert_eq!(request["params"]["useStateDbOnly"], true);
            assert_eq!(request["params"]["attachedToThreadId"], json!(parent));
            peer.json(&json!({"id":request["id"],"result":{"data":[],"nextCursor":null}}));
        }
    });
    let result = observe(
        server.path.to_str().unwrap(),
        &scope(),
        &expected(),
        &[LIVE.to_owned()],
    );
    assert_eq!(result["persistent_absence"], true);
    assert_eq!(
        result["complete_persistent_inventories"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        result["memory_observations"][0]["rpc_error"]["message"],
        format!("thread not loaded: {SESSION}")
    );
    server.finish();
}

#[test]
fn unloaded_but_persistent_active_or_archived_thread_is_never_deleted() {
    for present_in_archive in [false, true] {
        assert!(std::panic::catch_unwind(|| with_list(|params| Ok(json!({
            "data":if params["archived"] == present_in_archive { vec![row(SESSION,"/owned/checkout")] } else {vec![]},"nextCursor":null
        })))).is_err());
    }
}

#[test]
fn follows_complete_cursor_chain_and_allows_distinct_live_roles_in_same_owned_cwd() {
    let mut requests = Vec::new();
    let result = with_list(|params| {
        requests.push(params.clone());
        if params["archived"] == false && params["cursor"].is_null() {
            Ok(json!({"data":[row(LIVE,"/owned/checkout")],"nextCursor":"next-active"}))
        } else {
            Ok(json!({"data":[],"nextCursor":null}))
        }
    });
    assert_eq!(result["persistent_absence"], true);
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[1]["cursor"], "next-active");
    assert_eq!(requests[2]["archived"], true);
    assert!(requests[2]["cursor"].is_null());
}

#[test]
fn old_session_on_a_later_page_is_not_missed() {
    assert!(
        std::panic::catch_unwind(|| with_list(|params| Ok(if params["cursor"].is_null() {
            json!({"data":[],"nextCursor":"later"})
        } else {
            json!({"data":[row(SESSION,"/owned/checkout")],"nextCursor":null})
        })))
        .is_err()
    );
}

#[test]
fn malformed_wrong_scope_unknown_or_cyclic_cursor_and_page_overflow_fail_closed() {
    for reply in [
        json!({}),
        json!({"data":[],"nextCursor":false}),
        json!({"data":[]}),
        json!({"data":{},"nextCursor":null}),
        json!({"data":[],"nextCursor":""}),
        json!({"data":[{}],"nextCursor":null}),
        json!({"data":[{"id":LIVE}],"nextCursor":null}),
        json!({"data":[row(LIVE,"/foreign/checkout")],"nextCursor":null}),
        json!({"data":vec![row(LIVE,"/owned/checkout");129],"nextCursor":null}),
    ] {
        assert!(std::panic::catch_unwind(|| with_list(|_| Ok(reply.clone()))).is_err());
    }
    assert!(std::panic::catch_unwind(|| with_list(|_| Ok(
        json!({"data":[],"nextCursor":"repeated"})
    )))
    .is_err());
    let mut index = 0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| with_list(|_| {
            index += 1;
            Ok(json!({"data":[],"nextCursor":format!("page-{index}")}))
        })))
        .is_err()
    );
    assert_eq!(index, PAGE_BOUND);
}

#[test]
fn permission_transport_and_wrong_read_error_context_do_not_fall_back_to_absence() {
    for error in [
        herdr_flow::FlowError::rpc_timeout("timeout"),
        herdr_flow::FlowError::rpc("permission denied")
            .with_rpc(
                Some(-32001),
                Some(json!({"code":-32001,"message":"permission denied"})),
            )
            .with_method("thread/read"),
        unloaded(LIVE),
        unloaded(SESSION).with_method("thread/list"),
        unloaded(SESSION).with_rpc(Some(-32601), None),
        herdr_flow::FlowError::rpc(format!("thread not loaded: {SESSION}"))
            .with_rpc(Some(-32600), None)
            .with_method("thread/read"),
    ] {
        let mut remaining = Some(error);
        let mut calls = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prove(
                &scope(),
                &expected(),
                &[LIVE.to_owned()],
                &mut |_, _| {
                    calls += 1;
                    Err(remaining.take().unwrap())
                }
            )))
            .is_err()
        );
        assert_eq!(calls, 1);
    }
    assert!(
        std::panic::catch_unwind(|| with_list(|_| Err(herdr_flow::FlowError::rpc(
            "permission denied"
        )
        .with_method("thread/list"))))
        .is_err()
    );
    assert!(std::panic::catch_unwind(|| prove(
        &scope(),
        &expected(),
        &[LIVE.to_owned()],
        &mut |_, _| Ok(json!({"thread":{"id":SESSION,"status":{"type":"notLoaded"}}}))
    ))
    .is_err());
}

#[test]
fn refuses_empty_or_foreign_scope_before_any_inventory_request() {
    for owned in [
        Vec::new(),
        vec![PathBuf::from("relative")],
        vec![PathBuf::from("/foreign")],
    ] {
        let mut calls = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prove(
                &owned,
                &expected(),
                &[LIVE.to_owned()],
                &mut |_, _| {
                    calls += 1;
                    panic!("must reject scope")
                }
            )))
            .is_err()
        );
        assert_eq!(calls, 0);
    }
}

#[test]
fn attached_persistent_old_thread_is_not_hidden_by_empty_ordinary_listings() {
    for archived in [false, true] {
        assert!(std::panic::catch_unwind(|| with_list(|params| Ok(json!({"data":
            if params["attachedToThreadId"] == LIVE && params["archived"] == archived { vec![row(SESSION,"/owned/checkout")] } else {vec![]},
            "nextCursor":null})))).is_err());
    }
}

#[test]
fn legacy_exact_not_found_still_requires_complete_persistent_absence() {
    let mut list_calls = 0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prove(
            &scope(),
            &expected(),
            &[LIVE.to_owned()],
            &mut |method, _| {
                if method == "thread/read" {
                    let message = format!("thread not found: {SESSION}");
                    return Err(herdr_flow::FlowError::rpc(&message)
                        .with_rpc(Some(-32600), Some(json!({"code":-32600,"message":message})))
                        .with_method(method));
                }
                list_calls += 1;
                Ok(json!({"data":[row(SESSION,"/owned/checkout")],"nextCursor":null}))
            }
        )))
        .is_err()
    );
    assert_eq!(list_calls, 1);
}
