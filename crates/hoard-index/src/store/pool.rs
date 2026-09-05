use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};

use crate::error::{IndexError, Result};
use crate::store::migrations;

const CONNECTION_PRAGMAS: &str = "PRAGMA journal_mode = WAL; \
     PRAGMA foreign_keys = ON; \
     PRAGMA busy_timeout = 5000; \
     PRAGMA synchronous = NORMAL;";

/// A pooled connection for reading. SQLite allows any number of concurrent readers.
pub type Reader = PooledConnection<SqliteConnectionManager>;

/// The index database: pooled readers and a single serialized writer.
///
/// Every caller runs its closures on a blocking thread, either by holding [`Store::read`]
/// or [`Store::write`] inside `spawn_blocking`, or through [`Store::call`] and
/// [`Store::call_mut`] which do it for them.
#[derive(Clone)]
pub struct Store {
    pool: Pool<SqliteConnectionManager>,
    writer: Arc<Mutex<()>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("connections", &self.pool.state().connections)
            .finish()
    }
}

/// The exclusive write handle. Dereferences to the connection and releases the writer lock
/// when dropped.
pub struct Writer<'store> {
    _lock: MutexGuard<'store, ()>,
    connection: Reader,
}

impl fmt::Debug for Writer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Writer")
    }
}

impl Deref for Writer<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl DerefMut for Writer<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

impl Store {
    /// Opens the index at `path`, creating the parent directory and applying migrations.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| IndexError::io(parent, error))?;
        }
        let manager = SqliteConnectionManager::file(path).with_init(initialize_connection);
        Store::build(manager, None)
    }

    /// Opens a private in-memory index. The shared cache URI plus a single connection keeps
    /// every borrow looking at the same database.
    pub fn open_in_memory() -> Result<Store> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ordinal = COUNTER.fetch_add(1, Ordering::Relaxed);
        let uri = format!(
            "file:hoard-{}-{ordinal}?mode=memory&cache=shared",
            std::process::id()
        );
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let manager = SqliteConnectionManager::file(uri)
            .with_flags(flags)
            .with_init(initialize_connection);
        Store::build(manager, Some(1))
    }

    fn build(manager: SqliteConnectionManager, max_size: Option<u32>) -> Result<Store> {
        let mut builder = Pool::builder();
        if let Some(size) = max_size {
            builder = builder.max_size(size);
        }
        let store = Store {
            pool: builder.build(manager)?,
            writer: Arc::new(Mutex::new(())),
        };
        let mut connection = store.write()?;
        migrations::apply(&mut connection)?;
        drop(connection);
        Ok(store)
    }

    pub fn read(&self) -> Result<Reader> {
        Ok(self.pool.get()?)
    }

    /// Borrows the writer, blocking until any other writer has finished.
    pub fn write(&self) -> Result<Writer<'_>> {
        let lock = self.writer.lock();
        let connection = self.pool.get()?;
        Ok(Writer {
            _lock: lock,
            connection,
        })
    }

    /// Runs a read on a blocking thread.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let connection = store.read()?;
            f(&connection)
        })
        .await?
    }

    /// Runs a write on a blocking thread, serialized against every other writer.
    pub async fn call_mut<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = store.write()?;
            f(&mut connection)
        })
        .await?
    }
}

fn initialize_connection(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.execute_batch(CONNECTION_PRAGMAS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma(connection: &Connection, name: &str) -> String {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .map(|value| format!("{value:?}"))
            .expect("pragma reads")
    }

    #[test]
    fn every_connection_carries_the_index_pragmas() {
        let store = Store::open_in_memory().expect("store opens");
        let connection = store.read().expect("reader");
        assert_eq!(pragma(&connection, "foreign_keys"), "Integer(1)");
        assert_eq!(pragma(&connection, "busy_timeout"), "Integer(5000)");
    }

    #[test]
    fn a_file_database_uses_write_ahead_logging() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("hoard.db")).expect("store opens");
        let connection = store.read().expect("reader");
        assert_eq!(pragma(&connection, "journal_mode"), "Text(\"wal\")");
        assert_eq!(pragma(&connection, "synchronous"), "Integer(1)");
    }

    #[tokio::test]
    async fn call_and_call_mut_run_on_blocking_threads() {
        let store = Store::open_in_memory().expect("store opens");
        store
            .call_mut(|connection| {
                connection.execute("INSERT INTO setting(key, value) VALUES('a', 'b')", [])?;
                Ok(())
            })
            .await
            .expect("write runs");
        let value: String = store
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT value FROM setting WHERE key = 'a'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("read runs");
        assert_eq!(value, "b");
    }
}
