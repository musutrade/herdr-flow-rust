use super::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

impl Store {
    /// Register an existing real directory. Re-registration returns the original label/id.
    pub fn register_project(&mut self, root: &std::path::Path, label: &str) -> Result<Project> {
        self.check()?;
        validate::text(label, 1, 256)?;
        let root = project_root(root)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO projects(canonical_root,label) VALUES(?1,?2) ON CONFLICT(canonical_root) DO NOTHING", params![root,label])?;
        let p = tx.query_row(
            "SELECT id,canonical_root,label FROM projects WHERE canonical_root=?1",
            [root],
            super::query::project,
        )?;
        tx.commit()?;
        Ok(p)
    }

    pub fn apply(&mut self, request: &Request) -> Result<Receipt> {
        self.check()?;
        let body = validate::request(request)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = retry(&tx, &request.request_id, &body)? {
            return Ok(receipt);
        }
        commit_request(tx, request, &body)
    }
}

fn project_root(root: &std::path::Path) -> Result<String> {
    let root = std::fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(Error::Invalid("project root is not a directory".into()));
    }
    let text = root
        .to_str()
        .ok_or_else(|| Error::Invalid("project root must be UTF-8".into()))?;
    validate::text(text, 1, 4096)?;
    Ok(text.into())
}

fn commit_request(tx: rusqlite::Transaction<'_>, request: &Request, body: &str) -> Result<Receipt> {
    let now: String = tx.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
        r.get(0)
    })?;
    let mut receipt = mutate(&tx, request, &now)?;
    #[cfg(test)]
    super::tests::failures::after_mutation();
    receipt.event_id = next_event(&tx)?;
    append(&tx, request, body, &now, &receipt)?;
    tx.commit()?;
    Ok(receipt)
}

fn retry(conn: &Connection, id: &str, body: &str) -> Result<Option<Receipt>> {
    let old: Option<(String, String)> = conn
        .query_row(
            "SELECT request_json,receipt_json FROM events WHERE request_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match old {
        None => Ok(None),
        Some((b, receipt)) if b == body => Ok(Some(serde_json::from_str(&receipt)?)),
        Some(_) => Err(Error::RequestConflict),
    }
}

fn revision(conn: &Connection, task: i64, expected: i64) -> Result<i64> {
    let actual: i64 = conn
        .query_row("SELECT revision FROM tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or(Error::NotFound("task"))?;
    if expected != actual {
        return Err(Error::RevisionConflict { expected, actual });
    }
    actual
        .checked_add(1)
        .ok_or_else(|| Error::Invalid("revision overflow".into()))
}

fn mutate(conn: &Connection, r: &Request, now: &str) -> Result<Receipt> {
    match &r.change {
        Change::CreateTask { task } => create(conn, task, &r.summary, now),
        Change::Report {
            task_id,
            expected_revision,
            reported_state,
        } => report(
            conn,
            *task_id,
            *expected_revision,
            reported_state,
            &r.summary,
            now,
        ),
        Change::AttachBinding {
            task_id,
            expected_revision,
            binding,
        } => attach_change(conn, *task_id, *expected_revision, binding, &r.summary, now),
        Change::UpdateBinding {
            task_id,
            expected_revision,
            binding_id,
            binding,
        } => update_change(
            conn,
            *task_id,
            *expected_revision,
            *binding_id,
            binding,
            &r.summary,
            now,
        ),
        Change::SetResponsible {
            task_id,
            expected_revision,
            binding_id,
        } => responsible_change(
            conn,
            *task_id,
            *expected_revision,
            *binding_id,
            &r.summary,
            now,
        ),
    }
}
fn report(
    conn: &Connection,
    task: i64,
    expected: i64,
    state: &str,
    summary: &str,
    now: &str,
) -> Result<Receipt> {
    let revision = revision(conn, task, expected)?;
    conn.execute(
        "UPDATE tasks SET reported_state=?1 WHERE id=?2",
        params![state, task],
    )?;
    finish(conn, task, revision, None, summary, now)
}
fn attach_change(
    conn: &Connection,
    task: i64,
    expected: i64,
    binding: &BindingInput,
    summary: &str,
    now: &str,
) -> Result<Receipt> {
    let revision = revision(conn, task, expected)?;
    let id = attach(conn, task, binding, now)?;
    finish(conn, task, revision, Some(id), summary, now)
}
fn update_change(
    conn: &Connection,
    task: i64,
    expected: i64,
    id: i64,
    binding: &BindingInput,
    summary: &str,
    now: &str,
) -> Result<Receipt> {
    let revision = revision(conn, task, expected)?;
    owns_binding(conn, task, id)?;
    update_binding(conn, id, binding, now)?;
    finish(conn, task, revision, Some(id), summary, now)
}
fn responsible_change(
    conn: &Connection,
    task: i64,
    expected: i64,
    id: Option<i64>,
    summary: &str,
    now: &str,
) -> Result<Receipt> {
    let revision = revision(conn, task, expected)?;
    if let Some(id) = id {
        owns_binding(conn, task, id)?;
    }
    conn.execute(
        "UPDATE tasks SET responsible_binding_id=?1 WHERE id=?2",
        params![id, task],
    )?;
    finish(conn, task, revision, id, summary, now)
}

fn create(conn: &Connection, task: &NewTask, summary: &str, now: &str) -> Result<Receipt> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
        [task.project_id],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(Error::NotFound("project"));
    }
    conn.execute("INSERT INTO tasks(project_id,title,source_ref,batch_ref,reported_state,summary,revision,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,1,?7,?7)", params![task.project_id,task.title,task.source_ref,task.batch_ref,task.reported_state,summary,now])?;
    Ok(Receipt {
        event_id: 0,
        task_id: conn.last_insert_rowid(),
        revision: 1,
        binding_id: None,
    })
}

fn finish(
    conn: &Connection,
    task_id: i64,
    revision: i64,
    binding_id: Option<i64>,
    summary: &str,
    now: &str,
) -> Result<Receipt> {
    conn.execute(
        "UPDATE tasks SET summary=?1,revision=?2,updated_at=?3 WHERE id=?4",
        params![summary, revision, now, task_id],
    )?;
    Ok(Receipt {
        event_id: 0,
        task_id,
        revision,
        binding_id,
    })
}

fn attach(conn: &Connection, task: i64, b: &BindingInput, now: &str) -> Result<i64> {
    let location = serde_json::to_string(&b.herdr_location)?;
    let session = b.session.as_ref().map(serde_json::to_string).transpose()?;
    conn.execute("INSERT INTO bindings(task_id,role,herdr_location,session,cwd,recorded_at) VALUES(?1,?2,?3,?4,?5,?6)", params![task,b.role,location,session,b.cwd,now])?;
    Ok(conn.last_insert_rowid())
}

fn update_binding(conn: &Connection, id: i64, b: &BindingInput, now: &str) -> Result<()> {
    let location = serde_json::to_string(&b.herdr_location)?;
    let session = b.session.as_ref().map(serde_json::to_string).transpose()?;
    conn.execute("UPDATE bindings SET role=?1,herdr_location=?2,session=?3,cwd=?4,recorded_at=?5 WHERE id=?6", params![b.role,location,session,b.cwd,now,id])?;
    Ok(())
}

fn owns_binding(conn: &Connection, task: i64, binding: i64) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM bindings WHERE id=?1 AND task_id=?2)",
        params![binding, task],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(Error::NotFound("binding in this task"));
    }
    Ok(())
}

fn next_event(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(id),0)+1 FROM events", [], |r| r.get(0))?)
}

fn kind(change: &Change) -> &'static str {
    match change {
        Change::CreateTask { .. } => "create_task",
        Change::Report { .. } => "report",
        Change::AttachBinding { .. } => "attach_binding",
        Change::UpdateBinding { .. } => "update_binding",
        Change::SetResponsible { .. } => "set_responsible",
    }
}

fn append(conn: &Connection, r: &Request, body: &str, now: &str, receipt: &Receipt) -> Result<()> {
    conn.execute("INSERT INTO events(id,request_id,task_id,binding_id,kind,reporter,recorded_at,summary,refs,request_json,receipt_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![receipt.event_id,r.request_id,receipt.task_id,receipt.binding_id,kind(&r.change),r.reporter,now,r.summary,serde_json::to_string(&r.refs)?,body,serde_json::to_string(receipt)?])?;
    Ok(())
}
