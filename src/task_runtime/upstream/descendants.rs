//! Discover task descendants after their recorded parents have been deleted.
//! A shared cwd is only a search filter, never deletion authority.
use super::*;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

pub(crate) fn discover(sessions: &[J]) -> Result<Vec<J>> {
    discover_with(sessions, None, &mut scan)
}

/// One deadline covers every socket/cwd scope, both archive inventories, all
/// pages, and their RPC connection setup. Partial inventories are never used.
pub(crate) fn discover_bounded(sessions: &[J], timeout: Duration) -> Result<Vec<J>> {
    discover_with(sessions, Some(upstream_deadline(timeout)?), &mut scan)
}

type Scan<'a> = dyn FnMut(&Path, &str, &[J], Option<Instant>) -> Result<Vec<J>> + 'a;

fn discover_with(sessions: &[J], deadline: Option<Instant>, scan: &mut Scan<'_>) -> Result<Vec<J>> {
    check_deadline(deadline)?;
    let mut visited = BTreeSet::new();
    let mut found = vec![];
    for journal in sessions {
        check_deadline(deadline)?;
        let (socket, cwd) = scope(journal)?;
        if !visited.insert((socket.clone(), cwd.clone())) {
            continue;
        }
        found.extend(scan(&socket, &cwd, sessions, deadline)?);
        check_deadline(deadline)?;
    }
    ensure(found.len() <= 128, "upstream descendant bound exceeded")?;
    Ok(found)
}

fn scope(journal: &J) -> Result<(PathBuf, String)> {
    ensure(
        journal["phase"] == "deleted",
        "delete recorded parents before descendant discovery",
    )?;
    let runtime = &journal["runtime"];
    Ok((endpoint(runtime)?, required(runtime, "cwd")?))
}

fn scan(socket: &Path, cwd: &str, sessions: &[J], deadline: Option<Instant>) -> Result<Vec<J>> {
    let mut client = crate::codex::rpc::Client::new(&socket.to_string_lossy());
    let mut call = |method: &str, params: &J| match deadline {
        Some(deadline) => call_before(&mut client, method, params, deadline),
        None => client.call(method, params),
    };
    let rows = match deadline {
        Some(_) => list_before(cwd, deadline, &mut call),
        None => list(cwd, &mut call),
    }?;
    owned_rows(rows, cwd, sessions, deadline)
}

fn owned_rows(
    rows: Vec<J>,
    cwd: &str,
    sessions: &[J],
    deadline: Option<Instant>,
) -> Result<Vec<J>> {
    let mut found = vec![];
    for row in rows {
        check_deadline(deadline)?;
        if let Some(child) = owned(&row, cwd, sessions)? {
            found.push(child);
        }
        check_deadline(deadline)?;
    }
    Ok(found)
}

fn list(cwd: &str, call: &mut dyn FnMut(&str, &J) -> Result<J>) -> Result<Vec<J>> {
    list_before(cwd, None, call)
}

fn list_before(
    cwd: &str,
    deadline: Option<Instant>,
    call: &mut dyn FnMut(&str, &J) -> Result<J>,
) -> Result<Vec<J>> {
    let mut rows = vec![];
    for archived in [false, true] {
        let mut cursor = J::Null;
        for page in 0..8 {
            check_deadline(deadline)?;
            let reply = call(
                "thread/list",
                &json!({"cwd":cwd,"archived":archived,
                "sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentReview",
                    "subAgentCompact","subAgentThreadSpawn","subAgentOther","unknown"],
                "useStateDbOnly":true,"limit":128,"cursor":cursor}),
            )?;
            check_deadline(deadline)?;
            append_page(&mut rows, &reply)?;
            match next_cursor(&reply, &cursor, page)? {
                None => break,
                Some(next) => cursor = next,
            }
        }
    }
    Ok(rows)
}

fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if let Some(deadline) = deadline {
        remaining_budget(deadline)?;
    }
    Ok(())
}

fn next_cursor(reply: &J, cursor: &J, page: usize) -> Result<Option<J>> {
    let next = reply
        .get("nextCursor")
        .ok_or_else(|| FlowError::guard("missing thread-list cursor"))?;
    if next.is_null() {
        return Ok(None);
    }
    ensure(
        page < 7 && next.as_str().is_some_and(|s| !s.is_empty()) && *next != *cursor,
        "invalid or unbounded descendant pagination",
    )?;
    Ok(Some(next.clone()))
}

fn append_page(rows: &mut Vec<J>, reply: &J) -> Result<()> {
    let page = reply["data"]
        .as_array()
        .ok_or_else(|| FlowError::guard("invalid thread-list result"))?;
    ensure(page.len() <= 128, "oversized descendant page")?;
    rows.extend(page.iter().cloned());
    Ok(())
}

fn owned(row: &J, cwd: &str, sessions: &[J]) -> Result<Option<J>> {
    ensure(row["cwd"] == cwd, "upstream cwd filter mismatch")?;
    let id = required(row, "id")?;
    crate::identity::validate(&json!({"adapter":"codex","session":id}))?;
    if sessions.iter().any(|j| j["session"] == id) {
        return Err(FlowError::guard("deleted upstream session still listed"));
    }
    if row["source"].get("subAgent").is_none() {
        return Ok(None);
    }
    let metadata = metadata(row, &id)?;
    let parent = required(&metadata, "parent_thread_id")?;
    let Some(journal) = sessions.iter().find(|j| j["session"] == parent) else {
        return Ok(None);
    };
    ensure(
        journal["runtime"]["cwd"] == cwd,
        "descendant moved outside recorded cwd",
    )?;
    Ok(Some(
        json!({"phase":"created","session":id,"instance":journal["instance"],
        "runtime":journal["runtime"],"parent":parent,"source":"verified-rollout-metadata"}),
    ))
}

fn metadata(row: &J, id: &str) -> Result<J> {
    let path = PathBuf::from(required(row, "path")?);
    metadata_path(&path, id)?;
    let first = first_record(open_rollout(&path)?)?;
    validate_metadata(row, &first, id)?;
    Ok(first["payload"].clone())
}

fn metadata_path(path: &Path, id: &str) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    ensure(
        name.starts_with("rollout-") && name.ends_with(&format!("-{id}.jsonl")),
        "descendant rollout filename mismatch",
    )?;
    ensure(
        path.is_absolute() && std::fs::canonicalize(path)? == path,
        "descendant rollout must be canonical",
    )
}

fn open_rollout(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let stat = file.metadata()?;
    ensure(
        stat.is_file() && stat.uid() == util::process::current_uid(),
        "foreign descendant rollout",
    )?;
    Ok(file)
}

fn first_record(file: std::fs::File) -> Result<J> {
    let mut line = vec![];
    BufReader::new(file)
        .take(256 * 1024)
        .read_until(b'\n', &mut line)?;
    ensure(
        line.last() == Some(&b'\n'),
        "missing or oversized descendant metadata",
    )?;
    Ok(serde_json::from_slice(&line)?)
}

fn validate_metadata(row: &J, first: &J, id: &str) -> Result<()> {
    let meta = &first["payload"];
    ensure(
        first["type"] == "session_meta" && meta["id"] == id && meta["cwd"] == row["cwd"],
        "descendant metadata identity mismatch",
    )?;
    let parent = required(meta, "parent_thread_id")?;
    crate::identity::validate(&json!({"adapter":"codex","session":parent}))?;
    ensure(
        meta["source"].get("subagent").is_some(),
        "descendant metadata source mismatch",
    )?;
    if !row["parentThreadId"].is_null() {
        ensure(
            row["parentThreadId"] == parent,
            "conflicting descendant parent",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const PARENT: &str = "11111111-1111-1111-1111-111111111111";
    const CHILD: &str = "22222222-2222-2222-2222-222222222222";
    #[test]
    fn guardian_requires_exact_parent_and_metadata_not_shared_cwd() {
        let temp = util::fs::test_tempdir();
        let path = temp.path().join(format!("rollout-fixture-{CHILD}.jsonl"));
        let row = json!({"id":CHILD,"cwd":"/owned","path":path,"parentThreadId":null,
            "source":{"subAgent":{"other":"guardian"}}});
        let meta = json!({"type":"session_meta","payload":{"id":CHILD,"cwd":"/owned",
            "parent_thread_id":PARENT,"source":{"subagent":{"other":"guardian"}}}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let parent = json!({"phase":"deleted","session":PARENT,"instance":"ours","runtime":{"cwd":"/owned"}});
        let child = owned(&row, "/owned", std::slice::from_ref(&parent))
            .unwrap()
            .unwrap();
        assert_eq!(child["parent"], PARENT);
        assert_eq!(child["session"], CHILD);
        assert!(owned(&row, "/owned", &[]).unwrap().is_none());
        let mut deleted = parent.clone();
        deleted["session"] = json!(CHILD);
        assert!(owned(&row, "/owned", &[deleted]).is_err());
        assert!(owned(&row, "/other", std::slice::from_ref(&parent)).is_err());
        let mut wrong = row.clone();
        wrong["parentThreadId"] = json!(CHILD);
        assert!(owned(&wrong, "/owned", std::slice::from_ref(&parent)).is_err());
        wrong = row.clone();
        wrong["source"] = json!("cli");
        assert!(owned(&wrong, "/owned", std::slice::from_ref(&parent))
            .unwrap()
            .is_none());
        wrong = meta.clone();
        wrong["payload"]["id"] = json!(PARENT);
        assert!(validate_metadata(&row, &wrong, CHILD).is_err());
        wrong = meta.clone();
        wrong["payload"]["source"] = J::Null;
        assert!(validate_metadata(&row, &wrong, CHILD).is_err());
        let mut bad_path = row.clone();
        bad_path["path"] = json!(temp.path().join("unrelated.json"));
        assert!(metadata(&bad_path, CHILD).is_err());
        std::fs::write(&path, "no newline").unwrap();
        assert!(metadata(&row, CHILD).is_err());
        std::fs::write(&path, "{}\n").unwrap();
        assert!(owned(&row, "/owned", &[parent]).is_err());
    }

    #[test]
    fn pagination_and_archive_inventory_are_bounded() {
        let mut calls = 0;
        let rows = list("/owned", &mut |method, params| {
            assert_eq!(method, "thread/list");
            assert_eq!(params["cwd"], "/owned");
            calls += 1;
            Ok(json!({"data":[],"nextCursor":if calls == 1 {json!("next")} else {J::Null}}))
        })
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(calls, 3);
        for response in [
            json!({}),
            json!({"data":{}}),
            json!({"data":[]}),
            json!({"data":[],"nextCursor":"repeat"}),
        ] {
            assert!(list("/owned", &mut |_, _| Ok(response.clone())).is_err());
        }
    }

    #[test]
    fn bounded_discovery_shares_the_same_deadline_across_scopes() {
        let temp = util::fs::test_tempdir();
        let socket = temp.path().join("shared.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = socket_identity(&socket).unwrap();
        let sessions = ["/first", "/second"].map(|cwd| {
            json!({"phase":"deleted",
            "session":PARENT,"runtime":{"upstream_socket":identity,"cwd":cwd}})
        });
        let deadline = upstream_deadline(Duration::from_millis(80)).unwrap();
        let mut calls = 0;
        let error = discover_with(&sessions, Some(deadline), &mut |_, _, _, passed| {
            assert_eq!(passed, Some(deadline));
            calls += 1;
            if calls == 2 {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(vec![])
        })
        .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(error.kind, crate::ErrorKind::RpcTimeout);
        assert!(discover_bounded(&sessions, Duration::ZERO).is_err());
    }

    #[test]
    fn one_pagination_budget_covers_all_pages_and_both_archive_scans() {
        for expire_on in [1, 2] {
            let deadline = upstream_deadline(Duration::from_millis(80)).unwrap();
            let mut calls = 0;
            let error = list_before("/owned", Some(deadline), &mut |_, params| {
                calls += 1;
                assert_eq!(params["archived"], json!(calls == 2));
                if calls == expire_on {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(json!({"data":[],"nextCursor":null}))
            })
            .unwrap_err();
            assert_eq!(calls, expire_on);
            assert_eq!(error.kind, crate::ErrorKind::RpcTimeout);
        }
        let deadline = upstream_deadline(Duration::from_millis(80)).unwrap();
        let mut calls = 0;
        assert!(list_before("/owned", Some(deadline), &mut |_, params| {
            calls += 1;
            assert_eq!(params["archived"], false);
            std::thread::sleep(Duration::from_millis(100));
            Ok(json!({"data":[],"nextCursor":"next-page"}))
        })
        .is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn bounded_pagination_preserves_cursor_and_page_validation() {
        for response in [
            json!({}),
            json!({"data":{}}),
            json!({"data":[]}),
            json!({"data":[],"nextCursor":"repeat"}),
        ] {
            let deadline = upstream_deadline(Duration::from_secs(1)).unwrap();
            assert!(
                list_before("/owned", Some(deadline), &mut |_, _| Ok(response.clone())).is_err()
            );
        }
        let deadline = upstream_deadline(Duration::from_secs(1)).unwrap();
        let mut calls = 0;
        let rows = list_before("/owned", Some(deadline), &mut |_, _| {
            calls += 1;
            Ok(json!({"data":[],"nextCursor":if calls == 1 {json!("next")} else {J::Null}}))
        })
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(calls, 3);
    }
}
