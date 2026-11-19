pub mod db;
pub mod notion;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::ensure_parent;

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    read_conn: Arc<Mutex<Connection>>,
    uses_trigram: bool,
    path: Option<PathBuf>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite cache database: {}", path.display()))?;
        let uses_trigram = db::init_connection(&conn, false)?;
        let read_conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite reader connection: {}", path.display()))?;
        db::init_reader_connection(&read_conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            read_conn: Arc::new(Mutex::new(read_conn)),
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
            read_conn: shared,
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
        self.read_conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn shrink_memory(&self) {
        let conn = self.writer();
        db::shrink_connection_memory(&conn);
    }
}
