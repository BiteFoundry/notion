//! Standalone Notion SQLite WAL + FTS5 schema initialization.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 18;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 19;

/// Initializes SQLite WAL PRAGMAs, Notion workspace-scoped tables, indexes, and `pages_fts`.
/// Returns `true` if `pages_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    conn.set_prepared_statement_cache_capacity(64);
    if !is_memory {
        let _ = conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 67108864;
            ",
        );
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8000;
        ",
    )
    .context("Failed to configure SQLite PRAGMAs")?;

    if !is_memory {
        let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
        let user_version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        if user_version == SCHEMA_USER_VERSION_TRIGRAM
            || user_version == SCHEMA_USER_VERSION_UNICODE61
        {
            let intact: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pages_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pages')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='databases')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='blocks')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='block_children_fetched')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='comments')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='block_owners')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            icon TEXT,
            bot_id TEXT,
            bot_name TEXT,
            auth_mode TEXT NOT NULL DEFAULT 'integration',
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS pages (
            id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            parent_type TEXT NOT NULL DEFAULT 'workspace',
            parent_id TEXT,
            title TEXT NOT NULL,
            icon TEXT,
            cover_url TEXT,
            url TEXT NOT NULL DEFAULT '',
            archived INTEGER NOT NULL DEFAULT 0,
            is_favorite INTEGER NOT NULL DEFAULT 0,
            properties_json TEXT NOT NULL DEFAULT '{}',
            properties_summary TEXT NOT NULL DEFAULT '',
            plain_text TEXT NOT NULL DEFAULT '',
            created_time TEXT,
            last_edited_time TEXT,
            detail_synced_edited_time TEXT,
            comments_error TEXT,
            synced_at TEXT,
            PRIMARY KEY (workspace_id, id)
        );

        CREATE INDEX IF NOT EXISTS idx_pages_ws_parent
            ON pages(workspace_id, parent_type, parent_id);
        CREATE INDEX IF NOT EXISTS idx_pages_ws_edited
            ON pages(workspace_id, last_edited_time DESC);

        CREATE TABLE IF NOT EXISTS databases (
            id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            parent_type TEXT NOT NULL DEFAULT 'workspace',
            parent_id TEXT,
            title TEXT NOT NULL,
            icon TEXT,
            description TEXT NOT NULL DEFAULT '',
            schema_json TEXT NOT NULL DEFAULT '{}',
            url TEXT NOT NULL DEFAULT '',
            archived INTEGER NOT NULL DEFAULT 0,
            is_favorite INTEGER NOT NULL DEFAULT 0,
            last_edited_time TEXT,
            synced_at TEXT,
            PRIMARY KEY (workspace_id, id)
        );

        CREATE INDEX IF NOT EXISTS idx_databases_ws_edited
            ON databases(workspace_id, last_edited_time DESC);

        CREATE TABLE IF NOT EXISTS blocks (
            id TEXT NOT NULL,
            page_id TEXT NOT NULL,
            parent_block_id TEXT,
            workspace_id TEXT NOT NULL,
            position INTEGER NOT NULL DEFAULT 0,
            block_type TEXT NOT NULL,
            content_json TEXT NOT NULL,
            plain_text TEXT NOT NULL DEFAULT '',
            has_children INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (workspace_id, id)
        );

        CREATE INDEX IF NOT EXISTS idx_blocks_page_pos
            ON blocks(workspace_id, page_id, position ASC);

        CREATE TABLE IF NOT EXISTS block_children_fetched (
            workspace_id TEXT NOT NULL,
            parent_id TEXT NOT NULL,
            PRIMARY KEY (workspace_id, parent_id)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS comments (
            id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            page_id TEXT NOT NULL,
            discussion_id TEXT NOT NULL DEFAULT '',
            author TEXT NOT NULL,
            body TEXT NOT NULL,
            rich_text_json TEXT NOT NULL DEFAULT '[]',
            created_time TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (workspace_id, id)
        );

        CREATE INDEX IF NOT EXISTS idx_comments_page_time
            ON comments(workspace_id, page_id, created_time ASC);

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            cursor TEXT,
            last_synced_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS block_owners (
            workspace_id TEXT NOT NULL,
            block_id TEXT NOT NULL,
            owner_page_id TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, block_id)
        );
        ",
    )
    .context("Failed to create Notion SQLite schema")?;

    // Additive migration for pre-v14 databases: preserves all cached pages, blocks, comments, and FTS rows.
    ensure_column_exists(conn, "pages", "detail_synced_edited_time", "TEXT")?;
    if ensure_column_exists(conn, "pages", "comments_error", "TEXT")? {
        // Older caches stored unavailable comments as an empty list. Recheck those
        // pages on their first online read and leave cached content available offline.
        conn.execute_batch(
            "UPDATE pages SET detail_synced_edited_time = NULL
             WHERE detail_synced_edited_time IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM comments
                 WHERE comments.workspace_id = pages.workspace_id AND comments.page_id = pages.id
             )",
        )?;
    }

    let trigram = ensure_fts5_table(
        conn,
        "pages_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            page_id UNINDEXED,
            workspace_id UNINDEXED,
            item_type UNINDEXED,
            title,
            plain_text,
            properties_summary,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            page_id UNINDEXED,
            workspace_id UNINDEXED,
            item_type UNINDEXED,
            title,
            plain_text,
            properties_summary,
            tokenize='unicode61'
        );
        ",
    )?;

    if !is_memory {
        let version = if trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(trigram)
}

/// Configures a read-only WAL reader connection for concurrent UI reads.
pub fn init_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -4000;
        ",
    )
    .context("Failed to configure SQLite reader PRAGMAs")?;
    let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
    Ok(())
}

pub fn shrink_connection_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

fn ensure_column_exists(conn: &Connection, table: &str, column: &str, col_type: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table});"))?;
    let mut has_col = false;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows.flatten() {
        if name == column {
            has_col = true;
            break;
        }
    }
    if !has_col {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {col_type};"))
            .with_context(|| format!("Failed to add column {table}.{column}"))?;
    }
    Ok(!has_col)
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if let Ok(existing_sql) = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table_name],
        |row| row.get::<_, String>(0),
    ) {
        return Ok(existing_sql.to_ascii_lowercase().contains("trigram"));
    }

    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql).with_context(|| {
            format!("Failed to create FTS5 virtual table ({table_name}, unicode61)")
        })?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}
