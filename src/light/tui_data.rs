//! Read-only browser views reuse Store and Cache; there is no browser database.
use super::{observe_cache::Cache, Error, Options, Page, Result, Store};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum View {
    Projects,
    Tasks,
    Events,
    Bindings,
    Event(i64),
    Binding(i64),
}
pub(super) struct Browser {
    pub view: View,
    pub project: Option<i64>,
    pub batch: Option<String>,
    pub task: Option<i64>,
    pub page: Page,
    pub history: Vec<i64>,
    pub ids: Vec<i64>,
    pub next: Option<i64>,
}
impl Browser {
    pub fn reset(&mut self, view: View) {
        self.view = view;
        self.page.after_id = 0;
        self.history.clear();
        self.ids.clear();
        self.next = None;
    }
    pub fn load(&mut self, options: &Options) -> Result<Value> {
        self.ids.clear();
        self.next = None;
        let mut readonly = options.clone();
        readonly.read_only = true;
        let store = Store::open(readonly)?;
        // Keep multi-query record details in one SQLite read snapshot.
        store.conn.execute_batch("BEGIN;")?;
        let data = self.query(&store, options)?;
        store.conn.execute_batch("COMMIT;")?;
        Ok(data)
    }
    fn query(&mut self, store: &Store, options: &Options) -> Result<Value> {
        match self.view {
            View::Projects => self.rows(json!(store.projects(self.page)?)),
            View::Tasks => self.rows(json!(store.filtered_tasks(
                self.project,
                self.batch.as_deref(),
                self.page
            )?)),
            _ => self.details(store, options),
        }
    }
    fn details(&mut self, store: &Store, options: &Options) -> Result<Value> {
        let task = self.selected_task(store)?;
        let data = self.detail_data(store, options, task.id)?;
        Ok(json!({"task":task,"detail":data}))
    }
    fn selected_task(&self, store: &Store) -> Result<super::Task> {
        let task = store.task(self.task.ok_or(Error::NotFound("selected task"))?)?;
        if self.project.is_some_and(|id| task.project_id != id) {
            return Err(Error::NotFound("task in selected project"));
        }
        Ok(task)
    }
    fn detail_data(&mut self, store: &Store, options: &Options, task: i64) -> Result<Value> {
        match self.view {
            View::Events => self.rows(json!(store.events(task, self.page)?)),
            View::Bindings => self.rows(json!(store.bindings(task, self.page)?)),
            View::Event(id) => event(store, task, id),
            View::Binding(id) => binding(store, options, task, id),
            _ => Err(Error::Invalid("not a detail view".into())),
        }
    }
    fn rows(&mut self, rows: Value) -> Result<Value> {
        self.ids = rows
            .as_array()
            .ok_or(Error::Invalid("invalid row list".into()))?
            .iter()
            .filter_map(|r| r["id"].as_i64())
            .collect();
        self.next = (self.ids.len() == self.page.limit as usize)
            .then(|| self.ids.last().copied())
            .flatten();
        Ok(rows)
    }
}
fn event(store: &Store, task: i64, id: i64) -> Result<Value> {
    let rows = store.events(
        task,
        Page {
            after_id: id - 1,
            limit: 1,
        },
    )?;
    let found = rows
        .first()
        .filter(|r| r.id == id)
        .ok_or(Error::NotFound("event in task"))?;
    Ok(json!({"event":found}))
}
fn binding(store: &Store, options: &Options, task: i64, id: i64) -> Result<Value> {
    let binding = store.binding(id)?;
    if binding.task_id != task {
        return Err(Error::NotFound("binding in task"));
    }
    let observation = cached(options, &binding);
    Ok(json!({"binding":binding,"cached_observation":observation}))
}
fn cached(options: &Options, binding: &super::Binding) -> Value {
    match read_cache(options, binding) {
        Ok(value) => value,
        Err(error) => json!({"state":"unknown","cache_error":error.to_string()}),
    }
}
fn read_cache(options: &Options, binding: &super::Binding) -> Result<Value> {
    let cache = Cache::open(options, true)?;
    let Some(mut snapshot) = cache.get(binding.id)? else {
        return Ok(json!({"state":"unknown","reason":"no_cached_observation"}));
    };
    if snapshot.binding != *binding {
        return Ok(json!({"state":"unknown","reason":"binding_changed_since_observation"}));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64;
    let recorded_state = snapshot.observed_state.clone();
    let age = now.checked_sub(snapshot.observed_at_ms);
    snapshot.current(now);
    Ok(
        json!({"recorded_state":recorded_state,"age_ms":age,"fresh":age.is_some_and(|n|n<=snapshot.stale_after_ms),"snapshot":snapshot,"liveness":"not checked; cached observation only, not proof of current state"}),
    )
}
