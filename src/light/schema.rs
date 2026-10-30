use super::{Error, Result};
use rusqlite::{Connection, TransactionBehavior};

pub(super) const VERSION: i64 = 1;
const DDL: &str = r#"
CREATE TABLE projects (
 id INTEGER PRIMARY KEY,
 canonical_root TEXT NOT NULL UNIQUE CHECK(length(canonical_root) BETWEEN 1 AND 4096),
 label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 256)
) STRICT;
CREATE TABLE tasks (
 id INTEGER PRIMARY KEY,
 project_id INTEGER NOT NULL REFERENCES projects(id),
 title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 512),
 source_ref TEXT CHECK(length(source_ref) <= 4096),
 batch_ref TEXT CHECK(length(batch_ref) <= 256),
 reported_state TEXT NOT NULL CHECK(length(reported_state) BETWEEN 1 AND 128),
 summary TEXT NOT NULL CHECK(length(summary) <= 8192),
 responsible_binding_id INTEGER,
 revision INTEGER NOT NULL CHECK(revision >= 1),
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 FOREIGN KEY(responsible_binding_id,id) REFERENCES bindings(id,task_id)
) STRICT;
CREATE TABLE bindings (
 id INTEGER PRIMARY KEY,
 task_id INTEGER NOT NULL REFERENCES tasks(id),
 role TEXT NOT NULL CHECK(length(role) BETWEEN 1 AND 128),
 herdr_location TEXT NOT NULL CHECK(json_valid(herdr_location)),
 session TEXT CHECK(session IS NULL OR json_valid(session)),
 cwd TEXT NOT NULL CHECK(length(cwd) BETWEEN 1 AND 4096),
 recorded_at TEXT NOT NULL,
 UNIQUE(id,task_id)
) STRICT;
CREATE TABLE events (
 id INTEGER PRIMARY KEY,
 request_id TEXT NOT NULL UNIQUE CHECK(length(request_id) BETWEEN 1 AND 256),
 task_id INTEGER NOT NULL REFERENCES tasks(id),
 binding_id INTEGER,
 kind TEXT NOT NULL CHECK(kind IN ('create_task','report','attach_binding','update_binding','set_responsible')),
 reporter TEXT NOT NULL CHECK(length(reporter) BETWEEN 1 AND 256),
 recorded_at TEXT NOT NULL,
 summary TEXT NOT NULL CHECK(length(summary) <= 8192),
 refs TEXT NOT NULL CHECK(json_valid(refs) AND length(refs) <= 32768),
 request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(request_json) <= 65536),
 receipt_json TEXT NOT NULL CHECK(json_valid(receipt_json)),
 FOREIGN KEY(binding_id,task_id) REFERENCES bindings(id,task_id)
) STRICT;
CREATE INDEX tasks_project ON tasks(project_id,id);
CREATE INDEX tasks_batch ON tasks(project_id,batch_ref,id);
CREATE INDEX bindings_task ON bindings(task_id,id);
CREATE INDEX events_task ON events(task_id,id);
PRAGMA user_version=1;
"#;

pub(super) fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

pub(super) fn check(conn: &Connection) -> Result<()> {
    let v = version(conn)?;
    if v != VERSION {
        return Err(Error::UnsupportedSchema(v));
    }
    Ok(())
}

pub(super) fn initialize(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match version(&tx)? {
        VERSION => (),
        0 => create_empty(&tx)?,
        v => return Err(Error::UnsupportedSchema(v)),
    }
    tx.commit()?;
    Ok(())
}

fn create_empty(conn: &Connection) -> Result<()> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if n != 0 {
        return Err(Error::Invalid("unversioned database is not empty".into()));
    }
    conn.execute_batch(DDL)?;
    Ok(())
}
