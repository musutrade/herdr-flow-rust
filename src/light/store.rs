use super::{paths::Directory, schema, Error, Result};
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    pub data_root: PathBuf,
    pub forbidden_directories: Vec<PathBuf>,
    pub busy_timeout: Duration,
    pub read_only: bool,
}
impl Options {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            forbidden_directories: Vec::new(),
            busy_timeout: Duration::from_millis(250),
            read_only: false,
        }
    }
}

/// A local connection; mutations use short IMMEDIATE transactions.
/// Drop it between CLI calls. No background workers or business operations.
pub struct Store {
    pub(super) conn: Connection,
    directory: Directory,
}

impl Store {
    pub fn open(options: Options) -> Result<Self> {
        if options.busy_timeout > Duration::from_secs(5) {
            return Err(Error::Invalid("busy_timeout exceeds 5 seconds".into()));
        }
        let directory = Directory::open(
            &options.data_root,
            &options.forbidden_directories,
            !options.read_only,
        )?;
        let path = directory.file("records.sqlite3", !options.read_only)?;
        check_sidecars(&directory)?;
        let mut conn = connect(&path, options.read_only, options.busy_timeout)?;
        directory.verify()?;
        prepare(&mut conn, options.read_only)?;
        let store = Self { conn, directory };
        store.check()?;
        Ok(store)
    }

    pub fn database_path(&self) -> PathBuf {
        self.directory.path.join("records.sqlite3")
    }

    pub fn sqlite_version(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))?)
    }

    pub(super) fn check(&self) -> Result<()> {
        self.directory.verify()?;
        std::fs::symlink_metadata(self.database_path())?;
        self.directory.check_file("records.sqlite3")?;
        check_sidecars(&self.directory)?;
        schema::check(&self.conn)
    }

    /// Online SQLite backup into a new private directory. Never overwrites a DB.
    /// Reopen the returned root with Store::open to restore/use the snapshot.
    pub fn backup(&self, root: &Path, forbidden: &[PathBuf]) -> Result<()> {
        self.check()?;
        let mut excluded = self.directory.forbidden.clone();
        excluded.extend_from_slice(forbidden);
        excluded.push(self.directory.path.clone());
        let dir = Directory::open(root, &excluded, true)?;
        snapshot(&self.conn, &dir)
    }
}

fn snapshot(source: &Connection, dir: &Directory) -> Result<()> {
    let temporary = tempfile::NamedTempFile::new_in(dir.anchor())?;
    let filename = temporary
        .path()
        .file_name()
        .ok_or_else(|| Error::Invalid("backup filename missing".into()))?;
    let sqlite_path = dir.path.join(filename);
    dir.verify()?;
    let dest = connect(&sqlite_path, false, Duration::from_millis(250))?;
    dir.verify()?;
    write_snapshot(source, dest)?;
    publish_snapshot(temporary, dir)
}
fn write_snapshot(source: &Connection, mut dest: Connection) -> Result<()> {
    copy(source, &mut dest)?;
    verify_backup(&dest)?;
    dest.pragma_update(None, "journal_mode", "DELETE")?;
    dest.close().map_err(|(_, e)| Error::Sqlite(e))?;
    Ok(())
}
fn publish_snapshot(temporary: tempfile::NamedTempFile, dir: &Directory) -> Result<()> {
    temporary.as_file().sync_all()?;
    dir.verify()?;
    temporary
        .persist_noclobber(dir.anchor().join("records.sqlite3"))
        .map_err(|e| Error::Io(e.error))?;
    dir.sync()?;
    dir.check_file("records.sqlite3")
}

fn check_sidecars(directory: &Directory) -> Result<()> {
    for name in [
        "records.sqlite3-wal",
        "records.sqlite3-shm",
        "records.sqlite3-journal",
    ] {
        directory.check_file(name)?;
    }
    Ok(())
}

fn connect(path: &Path, readonly: bool, timeout: Duration) -> Result<Connection> {
    let mode = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let conn = Connection::open_with_flags(
        path,
        mode | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(timeout)?;
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;",
    )?;
    Ok(conn)
}

fn prepare(conn: &mut Connection, readonly: bool) -> Result<()> {
    if readonly {
        return schema::check(conn);
    }
    let version = schema::version(conn)?;
    if version != 0 && version != schema::VERSION {
        return Err(Error::UnsupportedSchema(version));
    }
    schema::initialize(conn)?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if mode != "wal" {
        return Err(Error::Invalid("WAL not enabled".into()));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn copy(source: &Connection, dest: &mut Connection) -> Result<()> {
    let backup = Backup::new(source, dest)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if backup.step(64)? == StepResult::Done {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(
                "backup did not finish within 5 seconds; destination is not a verified backup"
                    .into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn verify_backup(conn: &Connection) -> Result<()> {
    schema::check(conn)?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let foreign_errors: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if check != "ok" || foreign_errors != 0 {
        return Err(Error::Invalid("backup integrity check failed".into()));
    }
    Ok(())
}
