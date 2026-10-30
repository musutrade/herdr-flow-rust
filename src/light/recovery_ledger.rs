use super::{
    paths::Directory,
    recovery::{Authorization, State},
    Error, Options, Result,
};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

pub(super) struct Ledger {
    dir: Directory,
    conn: Connection,
    _lock: File,
}
impl Ledger {
    pub fn open(options: &Options, authorization: &Authorization) -> Result<Self> {
        // This is a distinct, explicitly selected private recovery root, not the
        // record store, a Codex internal database, or the observation cache.
        let dir = Directory::open(&options.data_root, &options.forbidden_directories, true)?;
        let lock = lock(&dir)?;
        let (conn, created) = connect(&dir, options)?;
        identity(&conn, authorization, created)?;
        check(&dir)?;
        dir.sync()?;
        Ok(Self {
            dir,
            conn,
            _lock: lock,
        })
    }
    pub fn load(&self) -> Result<State> {
        check(&self.dir)?;
        let state: String =
            self.conn
                .query_row("SELECT state FROM recovery WHERE id=1", [], |r| r.get(0))?;
        Ok(serde_json::from_str(&state)?)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        check(&self.dir)?;
        self.conn.execute(
            "UPDATE recovery SET state=?1 WHERE id=1",
            [serde_json::to_string(state)?],
        )?;
        check(&self.dir)?;
        self.dir.sync()
    }
}
fn check(dir: &Directory) -> Result<()> {
    dir.verify()?;
    for name in [
        "recovery.lock",
        "recovery.sqlite3",
        "recovery.sqlite3-journal",
        "recovery.sqlite3-wal",
        "recovery.sqlite3-shm",
    ] {
        dir.check_file(name)?;
    }
    Ok(())
}

fn lock(dir: &Directory) -> Result<File> {
    let existed = dir.path.join("recovery.lock").try_exists()?;
    let path = dir.file("recovery.lock", true)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // SAFETY: live descriptor owned by this process.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if existed && !dir.path.join("recovery.sqlite3").try_exists()? {
        return Err(Error::Invalid(
            "recovery ledger missing behind existing lock; refusing budget reset".into(),
        ));
    }
    Ok(file)
}
fn connect(dir: &Directory, options: &Options) -> Result<(Connection, bool)> {
    let exists = dir.path.join("recovery.sqlite3").try_exists()?;
    let path = dir.file("recovery.sqlite3", true)?;
    check(dir)?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(options.busy_timeout)?;
    initialize(&conn, exists)?;
    Ok((conn, !exists))
}
fn initialize(conn: &Connection, existed: bool) -> Result<()> {
    conn.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY; PRAGMA synchronous=FULL;",
    )?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 0 && version != 1 {
        return Err(Error::UnsupportedSchema(version));
    }
    if existed && version == 0 {
        return Err(Error::Invalid(
            "uninitialized existing recovery ledger; manual investigation required".into(),
        ));
    }
    conn.execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS recovery(id INTEGER PRIMARY KEY CHECK(id=1),authorization TEXT NOT NULL,state TEXT NOT NULL); PRAGMA user_version=1; COMMIT;")?;
    Ok(())
}
fn identity(conn: &Connection, authorization: &Authorization, created: bool) -> Result<()> {
    let identity = serde_json::to_string(authorization)?;
    if created {
        conn.execute(
            "INSERT INTO recovery VALUES(1,?1,?2)",
            rusqlite::params![identity, serde_json::to_string(&State::default())?],
        )?;
    }
    let stored: String =
        conn.query_row("SELECT authorization FROM recovery WHERE id=1", [], |r| {
            r.get(0)
        })?;
    if stored != identity {
        return Err(Error::Invalid(
            "recovery authorization identity drift; do not reset the ledger".into(),
        ));
    }
    Ok(())
}
