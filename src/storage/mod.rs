pub mod db;
pub mod notion;

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::ensure_parent;

const READER_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    read_pool: Arc<Vec<Arc<Mutex<Connection>>>>,
    read_next: Arc<AtomicUsize>,
    uses_trigram: bool,
    path: Option<PathBuf>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite cache database: {}", path.display()))?;
        let uses_trigram = db::init_connection(&conn, false)?;
        let mut readers = Vec::with_capacity(READER_POOL_SIZE);
        for _ in 0..READER_POOL_SIZE {
            let read_conn = Connection::open(path).with_context(|| {
                format!("Failed to open SQLite reader connection: {}", path.display())
            })?;
            db::init_reader_connection(&read_conn)?;
            readers.push(Arc::new(Mutex::new(read_conn)));
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            read_pool: Arc::new(readers),
            read_next: Arc::new(AtomicUsize::new(0)),
            uses_trigram,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory SQLite DB")?;
        let uses_trigram = db::init_connection(&conn, true)?;
        let shared = Arc::new(Mutex::new(conn));
        Ok(Self {
            conn: Arc::clone(&shared),
            read_pool: Arc::new(vec![shared]),
            read_next: Arc::new(AtomicUsize::new(0)),
            uses_trigram,
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn uses_trigram(&self) -> bool {
        self.uses_trigram
    }

    pub(crate) fn writer(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn reader(&self) -> MutexGuard<'_, Connection> {
        let len = self.read_pool.len();
        if len <= 1 {
            return self.read_pool[0]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let start = self.read_next.fetch_add(1, Ordering::Relaxed) % len;
        for offset in 0..len {
            let idx = (start + offset) % len;
            match self.read_pool[idx].try_lock() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => continue,
            }
        }
        self.read_pool[start]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs a blocking SQLite operation on Tokio's blocking thread pool so async workers
    /// (HTTP API, MCP, CLI) never stall on database I/O or lock contention.
    pub async fn blocking<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Storage) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let st = self.clone();
        tokio::task::spawn_blocking(move || f(&st))
            .await
            .map_err(|err| anyhow::anyhow!("SQLite worker task failed: {err}"))?
    }

    pub fn shrink_memory(&self) {
        {
            let conn = self.writer();
            db::shrink_connection_memory(&conn);
        }
        for reader in self.read_pool.iter() {
            if !Arc::ptr_eq(&self.conn, reader) {
                let conn = reader
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                db::shrink_connection_memory(&conn);
            }
        }
    }
}
