//! Exact owned-cwd durable inventories; an unloaded thread is not a deletion.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

const SOURCE_KINDS: &[&str] = &[
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];
const PAGE_LIMIT: usize = 128;
const PAGE_BOUND: usize = 8;
const TOTAL_PAGE_BOUND: usize = 128;

type Call<'a> = dyn FnMut(&str, &J) -> herdr_flow::Result<J> + 'a;

pub(super) fn observe(
    socket: &str,
    scope: &[PathBuf],
    expected: &BTreeMap<String, PathBuf>,
    parents: &[String],
) -> J {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut client = herdr_flow::codex::rpc::Client::new(socket);
    prove(scope, expected, parents, &mut |method, params| {
        client.timeout = remaining(deadline);
        client.connect()?;
        client.timeout = remaining(deadline);
        let reply = client.call(method, params);
        assert!(
            Instant::now() < deadline,
            "durable deletion audit RPC budget exhausted: {method}"
        );
        reply
    })
}

/// Complete ordinary and attached inventories for an exact private cwd.
/// Also used before terminal cleanup to retain Guardian/native child identities.
pub(super) fn snapshot(socket: &str, cwd: &Path, parents: &[String]) -> J {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut client = herdr_flow::codex::rpc::Client::new(socket);
    let mut pages = Vec::new();
    let mut call = |method: &str, params: &J| {
        client.timeout = remaining(deadline);
        client.connect()?;
        client.timeout = remaining(deadline);
        client.call(method, params)
    };
    for parent in std::iter::once(None).chain(parents.iter().map(Some)) {
        for archived in [false, true] {
            inventory(
                &[cwd.to_path_buf()],
                &BTreeMap::new(),
                archived,
                parent,
                &mut call,
                &mut pages,
            );
        }
    }
    json!(pages)
}

fn remaining(deadline: Instant) -> f64 {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64();
    assert!(
        remaining > 0.0,
        "durable deletion audit exhausted its aggregate budget"
    );
    remaining.min(5.0)
}

fn memory_observation(reply: herdr_flow::Result<J>, session: &str) -> J {
    match reply {
        Err(error) if error.kind == herdr_flow::ErrorKind::Rpc
            && error.method.as_deref() == Some("thread/read")
            && error.rpc_code == Some(-32600)
            && error.rpc_error.as_ref().is_some_and(|raw| raw["code"] == -32600 && raw["message"] == error.message)
            && [format!("thread not loaded: {session}"), format!("thread not found: {session}")].contains(&error.message) => {
                json!({"session":session,"kind":error.kind.type_name(),"method":error.method,
                    "rpc_code":error.rpc_code,"rpc_error":error.rpc_error,"message":error.message,
                    "meaning":"memory-only observation; durable absence requires both complete inventories"})
            }
        other => panic!("audit op=thread/read session={session}: unknown or still present upstream thread: {other:?}"),
    }
}

pub(super) fn prove(
    scope: &[PathBuf],
    expected: &BTreeMap<String, PathBuf>,
    parents: &[String],
    call: &mut Call<'_>,
) -> J {
    assert!(
        !scope.is_empty() && scope.len() <= 2 && scope.iter().all(|cwd| cwd.is_absolute()),
        "deletion audit requires exact nonempty owned cwd scope"
    );
    assert_eq!(
        scope.iter().collect::<BTreeSet<_>>().len(),
        scope.len(),
        "duplicate deletion audit cwd"
    );
    assert!(
        !expected.is_empty() && expected.len() <= 256,
        "invalid bound session inventory"
    );
    assert!(
        !parents.is_empty() && parents.len() <= 261,
        "invalid bound parent session inventory"
    );
    assert_eq!(
        parents.iter().collect::<BTreeSet<_>>().len(),
        parents.len(),
        "duplicate bound parent session"
    );
    for id in parents {
        herdr_flow::identity::validate(&json!({"adapter":"codex","session":id})).unwrap();
    }
    let mut memory = Vec::new();
    for (id, cwd) in expected {
        herdr_flow::identity::validate(&json!({"adapter":"codex","session":id})).unwrap();
        assert!(
            scope.contains(cwd),
            "bound session cwd is outside exact owned scope: {id}: {}",
            cwd.display()
        );
        memory.push(memory_observation(
            call("thread/read", &json!({"threadId":id,"includeTurns":false})),
            id,
        ));
    }
    let mut pages = Vec::new();
    // Ordinary listings intentionally hide attached threads in 0.165.0.
    // Inspect attachments to every caller-bound Owner parent as well.
    for parent in std::iter::once(None).chain(parents.iter().map(Some)) {
        for archived in [false, true] {
            inventory(scope, expected, archived, parent, call, &mut pages);
        }
    }
    json!({"scope":scope,"source_kinds":SOURCE_KINDS,"use_state_db_only":true,
        "known_sessions":expected,"bound_parent_sessions":parents,"memory_observations":memory,"complete_persistent_inventories":pages,
        "persistent_absence":true})
}

fn inventory(
    scope: &[PathBuf],
    expected: &BTreeMap<String, PathBuf>,
    archived: bool,
    parent: Option<&String>,
    call: &mut Call<'_>,
    pages: &mut Vec<J>,
) {
    let mut cursor = J::Null;
    let mut seen = BTreeSet::new();
    for page in 0..PAGE_BOUND {
        assert!(
            pages.len() < TOTAL_PAGE_BOUND,
            "audit thread/list aggregate page budget exhausted"
        );
        let mut params = json!({"cwd":scope,"archived":archived,"useStateDbOnly":true,
            "sourceKinds":SOURCE_KINDS,"modelProviders":[],"limit":PAGE_LIMIT,"cursor":cursor});
        if let Some(parent) = parent {
            params["attachedToThreadId"] = json!(parent);
        }
        let reply = call("thread/list", &params)
            .unwrap_or_else(|error| panic!("audit op=thread/list params={params}: {error:?}"));
        let rows = reply["data"]
            .as_array()
            .unwrap_or_else(|| panic!("audit op=thread/list params={params}: malformed data"));
        assert!(
            rows.len() <= PAGE_LIMIT,
            "audit op=thread/list params={params}: oversized page"
        );
        let mut ids = Vec::new();
        for row in rows {
            let id = row["id"].as_str().unwrap_or_else(|| {
                panic!("audit op=thread/list params={params}: missing string id")
            });
            herdr_flow::identity::validate(&json!({"adapter":"codex","session":id})).unwrap();
            let cwd = PathBuf::from(row["cwd"].as_str().unwrap_or_else(|| {
                panic!("audit op=thread/list params={params}: missing string cwd")
            }));
            assert!(
                scope.contains(&cwd),
                "audit op=thread/list params={params}: ignored exact cwd scope: {}",
                cwd.display()
            );
            assert!(!expected.contains_key(id), "audit thread/list archived={archived} cursor={cursor}: original session persists: {id}");
            ids.push(row.clone());
        }
        let next = reply.get("nextCursor").unwrap_or_else(|| {
            panic!("audit op=thread/list params={params}: next cursor is unknown")
        });
        pages.push(json!({"params":params,"rows":ids,"next_cursor":next}));
        if next.is_null() {
            return;
        }
        let next = next
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .unwrap_or_else(|| panic!("audit op=thread/list params={params}: invalid next cursor"));
        assert!(
            page + 1 < PAGE_BOUND && seen.insert(next.to_owned()),
            "audit op=thread/list params={params}: cyclic or incomplete pagination"
        );
        cursor = json!(next);
    }
    unreachable!("page bound rejects any unfinished inventory")
}

#[cfg(test)]
#[path = "stage_live_deletion_tests.rs"]
mod tests;
