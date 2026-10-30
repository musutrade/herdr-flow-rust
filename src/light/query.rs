use super::*;
use rusqlite::{params, OptionalExtension, Row};

impl Store {
    pub fn projects(&self, page: Page) -> Result<Vec<Project>> {
        self.check()?;
        validate::page(page)?;
        let mut q = self.conn.prepare(
            "SELECT id,canonical_root,label FROM projects WHERE id>?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = q.query_map(params![page.after_id, page.limit], project)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn task(&self, id: i64) -> Result<Task> {
        self.check()?;
        self.conn
            .query_row("SELECT * FROM tasks WHERE id=?1", [id], task)
            .optional()?
            .ok_or(Error::NotFound("task"))
    }

    /// All tasks in an explicitly selected project; optional batch label filter.
    pub fn tasks(&self, project_id: i64, batch: Option<&str>, page: Page) -> Result<Vec<Task>> {
        self.check()?;
        validate::page(page)?;
        let mut q = self.conn.prepare("SELECT * FROM tasks WHERE project_id=?1 AND (?2 IS NULL OR batch_ref=?2) AND id>?3 ORDER BY id LIMIT ?4")?;
        let rows = q.query_map(params![project_id, batch, page.after_id, page.limit], task)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn bindings(&self, task_id: i64, page: Page) -> Result<Vec<Binding>> {
        self.check()?;
        validate::page(page)?;
        let mut q = self
            .conn
            .prepare("SELECT * FROM bindings WHERE task_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let rows = q.query_map(params![task_id, page.after_id, page.limit], binding)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn events(&self, task_id: i64, page: Page) -> Result<Vec<Event>> {
        self.check()?;
        validate::page(page)?;
        let mut q = self
            .conn
            .prepare("SELECT * FROM events WHERE task_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let rows = q.query_map(params![task_id, page.after_id, page.limit], event)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

pub(super) fn project(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        canonical_root: r.get(1)?,
        label: r.get(2)?,
    })
}
fn task(r: &Row<'_>) -> rusqlite::Result<Task> {
    let core = task_core(r)?;
    Ok(Task {
        id: r.get(0)?,
        project_id: core.project_id,
        title: core.title,
        source_ref: core.source_ref,
        batch_ref: core.batch_ref,
        reported_state: core.reported_state,
        summary: r.get(6)?,
        responsible_binding_id: r.get(7)?,
        revision: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}
fn task_core(r: &Row<'_>) -> rusqlite::Result<NewTask> {
    Ok(NewTask {
        project_id: r.get(1)?,
        title: r.get(2)?,
        source_ref: r.get(3)?,
        batch_ref: r.get(4)?,
        reported_state: r.get(5)?,
    })
}
fn binding(r: &Row<'_>) -> rusqlite::Result<Binding> {
    let session: Option<String> = r.get(4)?;
    Ok(Binding {
        id: r.get(0)?,
        task_id: r.get(1)?,
        details: BindingInput {
            role: r.get(2)?,
            herdr_location: json(r, 3)?,
            session: session.map(|s| parse(&s, 4)).transpose()?,
            cwd: r.get(5)?,
        },
        recorded_at: r.get(6)?,
    })
}
fn event(r: &Row<'_>) -> rusqlite::Result<Event> {
    let request: Request = json(r, 9)?;
    Ok(Event {
        id: r.get(0)?,
        request_id: request.request_id.clone(),
        task_id: r.get(2)?,
        binding_id: r.get(3)?,
        kind: r.get(4)?,
        reporter: request.reporter.clone(),
        recorded_at: r.get(6)?,
        summary: request.summary.clone(),
        refs: request.refs.clone(),
        request,
        receipt: json(r, 10)?,
    })
}
fn json<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    parse(&row.get::<_, String>(index)?, index)
}
fn parse<T: serde::de::DeserializeOwned>(s: &str, index: usize) -> rusqlite::Result<T> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
