//! Minimal valid replies from the exact Codex 0.165.0 generated v2 schemas.
use crate::model::J;
use serde_json::json;

pub(crate) struct Case {
    pub method: &'static str,
    pub params: J,
    pub result: J,
}

pub(crate) fn metadata(thread: &str) -> Vec<Case> {
    [
        ("collaborationMode/list", json!({}), json!({"data":[]})),
        ("hooks/list", json!({"cwds":[]}), json!({"data":[]})),
        (
            "account/bedrock/checkGovCloudRequirements",
            json!({}),
            json!({"isGovCloud":false,"shouldWarn":false}),
        ),
        (
            "thread/items/list",
            json!({"threadId":thread}),
            json!({"data":[],"nextCursor":null,"backwardsCursor":null}),
        ),
        (
            "thread/goal/get",
            json!({"threadId":thread}),
            json!({"goal":null}),
        ),
        (
            "app/installed",
            json!({"threadId":thread,"forceRefresh":false}),
            json!({"apps":[]}),
        ),
        (
            "app/read",
            json!({"threadId":thread,"appIds":[],"includeTools":false}),
            json!({"apps":[],"missingAppIds":[]}),
        ),
    ]
    .into_iter()
    .map(|(method, params, result)| Case {
        method,
        params,
        result,
    })
    .collect()
}

pub(crate) fn command() -> Case {
    Case {
        method: "command/exec",
        params: json!({"command":["/usr/bin/true"],"processId":"catalog-command","streamStdoutStderr":true}),
        result: json!({"exitCode":0,"stdout":"","stderr":""}),
    }
}
