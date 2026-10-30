//! Private, bounded observation data, separate from authoritative records.sqlite3.
use super::{observe_state::Snapshot, paths::Directory, Error, Options, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
pub(super) struct Cache {
    conn: Connection,
    dir: Directory,
}
impl Cache {
    pub fn open(options: &Options, readonly: bool) -> Result<Self> {
        let dir = Directory::open(&options.data_root, &options.forbidden_directories, false)?;
        let path = dir.file("observations.sqlite3", !readonly)?;
        check_files(&dir)?;
        let conn = connect(&path, readonly, options.busy_timeout)?;
        if !readonly {
            initialize(&conn)?;
        }
        let result = Self { conn, dir };
        result.check()?;
        Ok(result)
    }
    fn check(&self) -> Result<()> {
        self.dir.verify()?;
        check_files(&self.dir)?;
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 1 {
            return Err(Error::UnsupportedSchema(version));
        }
        Ok(())
    }
    pub fn lease(&self, id: i64) -> Result<std::fs::File> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
        let name = format!("observe-{id}.lock");
        let path = self.dir.file(&name, true)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        self.dir.check_file(&name)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }
    pub fn get(&self, id: i64) -> Result<Option<Snapshot>> {
        self.check()?;
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT snapshot FROM latest WHERE binding_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        data.map(|s| {
            serde_json::from_str(&s).map_err(|_| Error::Invalid("invalid observation cache".into()))
        })
        .transpose()
    }
    pub fn put(&self, snapshot: &Snapshot) -> Result<()> {
        self.check()?;
        let json = serde_json::to_string(snapshot)
            .map_err(|_| Error::Invalid("observation encoding".into()))?;
        self.conn.execute("INSERT INTO latest(binding_id,snapshot) VALUES(?1,?2) ON CONFLICT(binding_id) DO UPDATE SET snapshot=excluded.snapshot,last_key=CASE WHEN ?3 THEN NULL ELSE latest.last_key END",params![snapshot.binding.id,json,matches!(snapshot.observed_state.as_str(),"working"|"idle")])?;
        self.check()
    }
    pub fn reserve(&mut self, id: i64, key: &str, now: u64) -> Result<(Option<i64>, &'static str)> {
        self.check()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reason) = suppressed(&tx, id, key, now)? {
            return Ok((None, reason));
        }
        tx.execute(
            "INSERT INTO notifications(binding_id,created_at_ms,status,reason) VALUES(?1,?2,'unknown','reserved_outcome_not_confirmed')",
            params![id, now.min(i64::MAX as u64) as i64],
        )?;
        let attempt = tx.last_insert_rowid();
        tx.execute(
            "UPDATE latest SET last_key=?2 WHERE binding_id=?1",
            params![id, key],
        )?;
        tx.execute("DELETE FROM notifications WHERE id NOT IN(SELECT id FROM notifications ORDER BY id DESC LIMIT 100)",[])?;
        tx.commit()?;
        Ok((Some(attempt), "reserved"))
    }
    pub fn finish(&self, id: i64, status: &str, reason: &str) -> Result<()> {
        self.check()?;
        self.conn.execute(
            "UPDATE notifications SET status=?2,reason=?3 WHERE id=?1",
            params![id, status, reason],
        )?;
        Ok(())
    }
    pub fn notifications(&self, binding: i64) -> Result<Value> {
        self.check()?;
        let mut statement=self.conn.prepare("SELECT id,created_at_ms,status,reason FROM notifications WHERE binding_id=?1 ORDER BY id DESC LIMIT 20")?;
        let rows=statement.query_map([binding],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"created_at_ms":r.get::<_,i64>(1)?,"status":r.get::<_,String>(2)?,"reason":r.get::<_,String>(3)?})))?;
        Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }
}
fn check_files(dir: &Directory) -> Result<()> {
    for name in [
        "observations.sqlite3",
        "observations.sqlite3-wal",
        "observations.sqlite3-shm",
        "observations.sqlite3-journal",
    ] {
        dir.check_file(name)?;
    }
    Ok(())
}
fn initialize(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 0 && version != 1 {
        return Err(Error::UnsupportedSchema(version));
    }
    conn.execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS latest(binding_id INTEGER PRIMARY KEY,snapshot TEXT NOT NULL,last_key TEXT); CREATE TABLE IF NOT EXISTS notifications(id INTEGER PRIMARY KEY AUTOINCREMENT,binding_id INTEGER NOT NULL,created_at_ms INTEGER NOT NULL,status TEXT NOT NULL,reason TEXT NOT NULL); PRAGMA user_version=1; COMMIT;")?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if mode != "wal" {
        return Err(Error::Invalid("observation WAL unavailable".into()));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn connect(
    path: &std::path::Path,
    readonly: bool,
    timeout: std::time::Duration,
) -> Result<Connection> {
    let flags = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let conn = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    conn.busy_timeout(timeout)?;
    conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;")?;
    Ok(conn)
}
fn suppressed(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    key: &str,
    now: u64,
) -> Result<Option<&'static str>> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT last_key FROM latest WHERE binding_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if previous.as_deref() == Some(key) {
        return Ok(Some("deduplicated"));
    }
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM notifications WHERE created_at_ms>=?1",
        [now.saturating_sub(3600000).min(i64::MAX as u64) as i64],
        |r| r.get(0),
    )?;
    Ok((count >= 3).then_some("rate_limited"))
}
