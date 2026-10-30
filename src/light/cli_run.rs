use super::{
    cli_args::{Command, Invocation},
    cli_input,
    cli_output::{CliResult, Failure},
    Error, Page, Store, Task,
};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn execute(inv: Invocation) -> CliResult<Value> {
    match inv.command {
        Command::Init => init(inv),
        Command::Observe => super::observe::run(inv),
        Command::Register => register(inv),
        Command::Projects => projects(inv),
        Command::Backup => backup(inv),
        _ => records(inv),
    }
}
fn records(inv: Invocation) -> CliResult<Value> {
    match inv.command {
        Command::Add | Command::Report | Command::Attach => write(inv),
        Command::Tasks => tasks(inv),
        Command::Task => task(inv),
        Command::Bindings => bindings(inv),
        _ => Err(Failure::usage("unsupported record command")),
    }
}
fn open(inv: &Invocation, readonly: bool) -> CliResult<Store> {
    // init is the only command allowed to create a missing store.
    std::fs::symlink_metadata(inv.options.data_root.join("records.sqlite3"))?;
    let mut options = inv.options.clone();
    options.read_only = readonly;
    Ok(Store::open(options)?)
}
fn init(inv: Invocation) -> CliResult<Value> {
    let store = Store::open(inv.options)?;
    Ok(
        json!({"database_path":store.database_path(),"schema_version":1,"sqlite_version":store.sqlite_version()?}),
    )
}
fn register(inv: Invocation) -> CliResult<Value> {
    let root = inv.required("--root")?;
    let label = inv.required("--label")?;
    let mut store = open(&inv, false)?;
    Ok(json!(store.register_project(Path::new(root), label)?))
}
fn projects(inv: Invocation) -> CliResult<Value> {
    let page = inv.page()?;
    let store = open(&inv, true)?;
    let rows = store.projects(page)?;
    let last = rows.last().map(|r| r.id);
    Ok(page_json(&rows, last, page))
}
fn write(inv: Invocation) -> CliResult<Value> {
    let request = cli_input::request(inv.required("--input")?, inv.command)?;
    let mut store = open(&inv, false)?;
    Ok(json!(store.apply(&request)?))
}
fn tasks(inv: Invocation) -> CliResult<Value> {
    let project = inv.id("--project")?;
    let page = inv.page()?;
    let store = open(&inv, true)?;
    let rows = store.tasks(project, inv.flags.get("--batch").map(String::as_str), page)?;
    let last = rows.last().map(|r| r.id);
    Ok(page_json(&rows, last, page))
}
fn scoped_task(store: &Store, project: i64, id: i64) -> CliResult<Task> {
    let task = store.task(id)?;
    if task.project_id != project {
        return Err(Error::NotFound("task in this project").into());
    }
    Ok(task)
}
fn selection(inv: &Invocation) -> CliResult<(i64, i64, Page)> {
    Ok((inv.id("--project")?, inv.id("--task")?, inv.page()?))
}
fn task(inv: Invocation) -> CliResult<Value> {
    let (project, id, page) = selection(&inv)?;
    let store = open(&inv, true)?;
    let task = scoped_task(&store, project, id)?;
    let events = store.events(id, page)?;
    let last = events.last().map(|r| r.id);
    Ok(json!({"task":task,"events":page_json(&events,last,page)}))
}
fn bindings(inv: Invocation) -> CliResult<Value> {
    let (project, id, page) = selection(&inv)?;
    let store = open(&inv, true)?;
    if !binding_scope(&store, project, id)? {
        return Ok(page_json::<super::Binding>(&[], None, page));
    }
    let rows = store.bindings(id, page)?;
    let last = rows.last().map(|r| r.id);
    Ok(page_json(&rows, last, page))
}
fn backup(inv: Invocation) -> CliResult<Value> {
    let root = Path::new(inv.required("--output-root")?);
    let store = open(&inv, true)?;
    store.backup(root, &inv.options.forbidden_directories)?;
    Ok(json!({"backup_root":std::fs::canonicalize(root)?}))
}
fn page_json<T: serde::Serialize>(rows: &[T], last: Option<i64>, page: Page) -> Value {
    let next = if rows.len() == page.limit as usize {
        last
    } else {
        None
    };
    json!({"items":rows,"next_after_id":next,"limit":page.limit})
}

fn binding_scope(store: &Store, project: i64, id: i64) -> CliResult<bool> {
    match scoped_task(store, project, id) {
        Ok(_) => Ok(true),
        Err(Failure::Store(Error::NotFound("task"))) => Ok(false),
        Err(error) => Err(error),
    }
}
