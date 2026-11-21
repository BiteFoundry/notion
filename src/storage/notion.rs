//! Workspace-scoped Notion SQLite CRUD + FTS5 search with Korean `<3` char `LIKE` fallback.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::{
    config::WorkspaceProfile,
    models::{
        NotionBlock, NotionComment, NotionDatabase, NotionPage, NotionPageDetail,
        NotionPageSummary, NotionPropertySchema, NotionPropertyValue, NotionSearchHit,
    },
    parser::{blocks_to_markdown, blocks_to_plain_text, collect_media_refs},
    storage::Storage,
};

pub(crate) enum CachedComments {
    NotFetched,
    Failed(String),
    Complete(Vec<NotionComment>),
    Partial(Vec<NotionComment>),
}

pub(crate) struct CommentsSummary {
    /// "cached" | "partial" | "failed" | "not_fetched"
    pub state: &'static str,
    pub count: Option<usize>,
    pub error: Option<String>,
}

impl Storage {
    pub fn upsert_workspace(&self, profile: &WorkspaceProfile) -> Result<()> {
        let conn = self.writer();
        conn.execute(
            "INSERT INTO workspaces (id, name, icon, bot_id, bot_name, auth_mode, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                icon = excluded.icon,
                bot_id = excluded.bot_id,
                bot_name = excluded.bot_name,
                auth_mode = excluded.auth_mode,
                updated_at = excluded.updated_at",
            params![
                profile.effective_workspace_id(),
                profile.display_name(),
                profile.workspace_icon,
                profile.bot_id,
                profile.bot_name,
                profile.auth_mode,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_pages(&self, pages: &[NotionPage]) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        for page in pages {
            upsert_page_in_tx(&tx, page)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_page(&self, page: &NotionPage) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        upsert_page_in_tx(&tx, page)?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_databases(&self, dbs: &[NotionDatabase]) -> Result<()> {
        if dbs.is_empty() {
            return Ok(());
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        for db in dbs {
            upsert_database_in_tx(&tx, db)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_database(&self, db: &NotionDatabase) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        upsert_database_in_tx(&tx, db)?;
        tx.commit()?;
        Ok(())
    }

    /// Persists the children of a page or block and updates the owning page's search index.
    pub fn replace_page_blocks(
        &self,
        workspace_id: &str,
        page_id: &str,
        blocks: &[NotionBlock],
    ) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        if let Some((owner_id, _)) = cached_child_parent(&tx, workspace_id, page_id)? {
            replace_nested_children(&tx, workspace_id, &owner_id, page_id, blocks)?;
        } else {
            replace_page_blocks_in_tx(&tx, workspace_id, page_id, blocks)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn replace_page_comments(
        &self,
        workspace_id: &str,
        page_id: &str,
        comments: &[NotionComment],
        complete: bool,
    ) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        replace_page_comments_in_tx(&tx, workspace_id, page_id, comments)?;
        let fetched: i64 = if complete { 1 } else { 2 };
        tx.execute(
            "UPDATE pages SET comments_error = NULL, comments_fetched = ?1 WHERE workspace_id = ?2 AND id = ?3",
            params![fetched, workspace_id, page_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Saves a full `NotionPageDetail` atomically (page metadata + blocks + comments + FTS5)
    /// and records `detail_synced_edited_time` only after blocks and comments are stored.
    pub fn save_page_detail(&self, detail: &NotionPageDetail) -> Result<()> {
        self.store_page_detail(detail, false)?;
        Ok(())
    }

    /// Stores page metadata and blocks only. Leaves comments, comments_error, comments_fetched
    /// and detail_synced_edited_time untouched.
    pub fn save_page_body(&self, page: &NotionPage, blocks: &[NotionBlock]) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        upsert_page_in_tx(&tx, page)?;
        replace_page_blocks_in_tx(&tx, &page.workspace_id, &page.id, blocks)?;
        let body_synced = page
            .last_edited_time
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        tx.execute(
            "UPDATE pages SET body_synced_edited_time = ?1 WHERE workspace_id = ?2 AND id = ?3",
            params![body_synced, page.workspace_id, page.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns the remote `last_edited_time` recorded when blocks were last fetched via compact sync.
    pub fn get_page_body_synced_edited_time(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.reader();
        let ts: Option<String> = conn
            .query_row(
                "SELECT body_synced_edited_time FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, page_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .filter(|s| !s.trim().is_empty());
        Ok(ts)
    }

    /// Comment state and count from existing markers; reads no comment bodies.
    pub(crate) fn cached_comments_summary(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<CommentsSummary> {
        let conn = self.reader();
        let row: Option<(i64, Option<String>, i64)> = conn
            .query_row(
                "SELECT comments_fetched, comments_error,
                        (SELECT COUNT(*) FROM comments
                         WHERE workspace_id = ?1 AND page_id = ?2)
                 FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, page_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            None => CommentsSummary {
                state: "not_fetched",
                count: None,
                error: None,
            },
            Some((_, Some(err), _)) => CommentsSummary {
                state: "failed",
                count: None,
                error: Some(err),
            },
            Some((1, None, count)) => CommentsSummary {
                state: "cached",
                count: Some(count.max(0) as usize),
                error: None,
            },
            Some((2, None, _)) => CommentsSummary {
                state: "partial",
                count: None,
                error: None,
            },
            Some((_, None, _)) => CommentsSummary {
                state: "not_fetched",
                count: None,
                error: None,
            },
        })
    }

    /// Records a fresh poll and returns whether it replaced the cached detail.
    pub(crate) fn save_page_detail_if_changed(&self, detail: &NotionPageDetail) -> Result<bool> {
        self.store_page_detail(detail, true)
    }

    fn store_page_detail(&self, detail: &NotionPageDetail, check_current: bool) -> Result<bool> {
        let mut conn = self.writer();
        let tx = if check_current {
            conn.transaction_with_behavior(TransactionBehavior::Immediate)?
        } else {
            conn.transaction()?
        };
        if check_current && page_detail_matches(&tx, detail)? {
            tx.execute(
                "UPDATE pages SET synced_at = ?1 WHERE workspace_id = ?2 AND id = ?3",
                params![chrono::Utc::now().to_rfc3339(), detail.page.workspace_id, detail.page.id],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        upsert_page_in_tx(&tx, &detail.page)?;
        replace_page_blocks_in_tx(
            &tx,
            &detail.page.workspace_id,
            &detail.page.id,
            &detail.blocks,
        )?;
        if detail.comments_error.is_none() {
            replace_page_comments_in_tx(
                &tx,
                &detail.page.workspace_id,
                &detail.page.id,
                &detail.comments,
            )?;
            let comments_fetched: i64 =
                if detail.comments.len() == crate::notion::DETAIL_COMMENTS_LIMIT + 1 {
                    2
                } else {
                    1
                };
            tx.execute(
                "UPDATE pages SET comments_fetched = ?1 WHERE workspace_id = ?2 AND id = ?3",
                params![comments_fetched, detail.page.workspace_id, detail.page.id],
            )?;
        }
        let synced_edited = detail
            .page
            .last_edited_time
            .as_deref()
            .filter(|s| !s.trim().is_empty() && detail.comments_error.is_none());
        tx.execute(
            "UPDATE pages SET detail_synced_edited_time = ?1, comments_error = ?2
             WHERE workspace_id = ?3 AND id = ?4",
            params![synced_edited, detail.comments_error, detail.page.workspace_id, detail.page.id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Returns the remote `last_edited_time` recorded when blocks and comments were last fetched.
    /// `None` means the page detail has never been synced (or was invalidated / migrated).
    pub fn get_page_detail_synced_edited_time(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.reader();
        detail_sync_marker(&conn, workspace_id, page_id)
    }

    /// Invalidates only the body sync marker for a single page so the next online read refetches
    /// blocks & comments, while preserving cached blocks for offline reads.
    pub fn invalidate_page_detail(&self, workspace_id: &str, page_id: &str) -> Result<()> {
        let conn = self.writer();
        conn.execute(
            "UPDATE pages SET detail_synced_edited_time = NULL, body_synced_edited_time = NULL WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, page_id],
        )?;
        Ok(())
    }

    /// Applies a single server-returned block update (e.g. to-do toggle or text patch) into SQLite,
    /// refreshing `pages.plain_text` and `pages_fts`.
    /// Returns `Ok(Some((page_id, needs_refetch)))` when a target page is known.
    /// When the response alone cannot restore the full block tree (missing cached row or newly
    /// reported children), invalidates that page's `detail_synced_edited_time` and sets `needs_refetch = true`.
    pub fn apply_updated_block(
        &self,
        workspace_id: &str,
        block: &NotionBlock,
    ) -> Result<Option<(String, bool)>> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;

        let existing: Option<(String, Option<String>, i64)> = tx
            .query_row(
                "SELECT page_id, parent_block_id, has_children FROM blocks WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, block.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((page_id, parent_block_id, existing_has_children)) = existing {
            if block.archived {
                delete_block_subtree_in_tx(&tx, workspace_id, &block.id)?;
                recompute_page_plain_text_in_tx(&tx, workspace_id, &page_id)?;
                tx.commit()?;
                return Ok(Some((page_id, false)));
            }

            let mut flat = block.clone();
            flat.page_id = page_id.clone();
            flat.parent_block_id = parent_block_id;
            flat.children.clear();
            let content_json = serde_json::to_string(&flat).unwrap_or_else(|_| "{}".into());
            let plain = flat.plain_text();
            let has_children_i = i64::from(block.has_children || existing_has_children != 0);

            tx.execute(
                "UPDATE blocks
                 SET block_type = ?1, content_json = ?2, plain_text = ?3, has_children = ?4
                 WHERE workspace_id = ?5 AND id = ?6",
                params![
                    block.block_type,
                    content_json,
                    plain,
                    has_children_i,
                    workspace_id,
                    block.id,
                ],
            )?;
            recompute_page_plain_text_in_tx(&tx, workspace_id, &page_id)?;

            let needs_refetch = block.has_children && existing_has_children == 0;
            if needs_refetch {
                tx.execute(
                    "UPDATE pages SET detail_synced_edited_time = NULL, body_synced_edited_time = NULL WHERE workspace_id = ?1 AND id = ?2",
                    params![workspace_id, page_id],
                )?;
            }
            tx.commit()?;
            return Ok(Some((page_id, needs_refetch)));
        }

        // Block is not cached yet; invalidate owning page detail if known so next read refetches.
        if !block.page_id.trim().is_empty() {
            tx.execute(
                "UPDATE pages SET detail_synced_edited_time = NULL, body_synced_edited_time = NULL WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, block.page_id],
            )?;
            tx.commit()?;
            return Ok(Some((block.page_id.clone(), true)));
        }

        tx.commit()?;
        Ok(None)
    }

    /// Removes a deleted/archived block (and any nested descendants) from the SQLite cache,
    /// recomputing `pages.plain_text` and `pages_fts`. Returns the owning `page_id` if cached.
    pub fn delete_cached_block(&self, workspace_id: &str, block_id: &str) -> Result<Option<String>> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let page_id: Option<String> = tx
            .query_row(
                "SELECT page_id FROM blocks WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, block_id],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(pid) = page_id {
            delete_block_subtree_in_tx(&tx, workspace_id, block_id)?;
            recompute_page_plain_text_in_tx(&tx, workspace_id, &pid)?;
            tx.commit()?;
            return Ok(Some(pid));
        }

        tx.commit()?;
        Ok(None)
    }

    /// Marks a single page as archived and removes its `pages_fts` row after authoritative
    /// confirmation from `GET /v1/pages/{id}` (archived/in_trash or HTTP 404 `object_not_found`).
    pub fn mark_page_archived(&self, workspace_id: &str, page_id: &str) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE pages SET archived = 1 WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, page_id],
        )?;
        tx.execute(
            "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
            params![workspace_id, page_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Marks a single database as archived and removes its `pages_fts` row after authoritative
    /// confirmation from `GET /v1/databases/{id}` (archived/in_trash or HTTP 404 `object_not_found`).
    pub fn mark_database_archived(&self, workspace_id: &str, database_id: &str) -> Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE databases SET archived = 1 WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, database_id],
        )?;
        tx.execute(
            "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
            params![workspace_id, database_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reconciles a complete, unfiltered database query against SQLite, marking rows absent from
    /// the remote query as archived and pruning them from `pages_fts`.
    pub fn reconcile_database_rows_archived(
        &self,
        workspace_id: &str,
        database_id: &str,
        active_row_ids: &HashSet<String>,
    ) -> Result<usize> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;

        let cached_rows: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM pages
                 WHERE workspace_id = ?1 AND parent_type = 'database_id' AND parent_id = ?2 AND archived = 0",
            )?;
            let ids = stmt
                .query_map(params![workspace_id, database_id], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };
        let mut archived_rows = 0usize;
        for rid in cached_rows {
            if !active_row_ids.contains(&rid) {
                tx.execute(
                    "UPDATE pages SET archived = 1 WHERE workspace_id = ?1 AND id = ?2",
                    params![workspace_id, rid],
                )?;
                tx.execute(
                    "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
                    params![workspace_id, rid],
                )?;
                archived_rows += 1;
            }
        }

        tx.commit()?;
        Ok(archived_rows)
    }

    pub fn toggle_page_favorite(&self, workspace_id: &str, page_id: &str) -> Result<bool> {
        let conn = self.writer();
        let current: bool = conn
            .query_row(
                "SELECT is_favorite FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, page_id],
                |r| Ok(r.get::<_, i64>(0)? != 0),
            )
            .optional()?
            .unwrap_or(false);
        let next = !current;
        conn.execute(
            "UPDATE pages SET is_favorite = ?1 WHERE workspace_id = ?2 AND id = ?3",
            params![i64::from(next), workspace_id, page_id],
        )?;
        Ok(next)
    }

    pub fn toggle_database_favorite(&self, workspace_id: &str, database_id: &str) -> Result<bool> {
        let conn = self.writer();
        let current: bool = conn
            .query_row(
                "SELECT is_favorite FROM databases WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, database_id],
                |r| Ok(r.get::<_, i64>(0)? != 0),
            )
            .optional()?
            .unwrap_or(false);
        let next = !current;
        conn.execute(
            "UPDATE databases SET is_favorite = ?1 WHERE workspace_id = ?2 AND id = ?3",
            params![i64::from(next), workspace_id, database_id],
        )?;
        Ok(next)
    }

    pub fn list_pages(&self, workspace_id: &str, include_archived: bool) -> Result<Vec<NotionPage>> {
        let conn = self.reader();
        let sql = if include_archived {
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, cover_url, url,
                    archived, is_favorite, properties_json, plain_text, created_time, last_edited_time, synced_at
             FROM pages WHERE workspace_id = ?1
             ORDER BY is_favorite DESC, last_edited_time DESC, title ASC"
        } else {
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, cover_url, url,
                    archived, is_favorite, properties_json, plain_text, created_time, last_edited_time, synced_at
             FROM pages WHERE workspace_id = ?1 AND archived = 0
             ORDER BY is_favorite DESC, last_edited_time DESC, title ASC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![workspace_id], map_page_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub(crate) fn list_page_summaries(&self, workspace_id: &str) -> Result<Vec<NotionPageSummary>> {
        let conn = self.reader();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, url,
                    archived, is_favorite, properties_summary, last_edited_time
             FROM pages WHERE workspace_id = ?1
             ORDER BY is_favorite DESC, last_edited_time DESC, title ASC",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok(NotionPageSummary {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                parent_type: r.get(2)?,
                parent_id: r.get(3)?,
                title: r.get(4)?,
                icon: r.get(5)?,
                url: r.get(6)?,
                archived: r.get::<_, i64>(7)? != 0,
                is_favorite: r.get::<_, i64>(8)? != 0,
                properties_summary: r.get(9)?,
                last_edited_time: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_database_rows(&self, workspace_id: &str, database_id: &str) -> Result<Vec<NotionPage>> {
        let conn = self.reader();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, cover_url, url,
                    archived, is_favorite, properties_json, plain_text, created_time, last_edited_time, synced_at
             FROM pages
             WHERE workspace_id = ?1 AND parent_type = 'database_id' AND parent_id = ?2 AND archived = 0
             ORDER BY last_edited_time DESC, title ASC",
        )?;
        let rows = stmt.query_map(params![workspace_id, database_id], map_page_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_page(&self, workspace_id: &str, page_id: &str) -> Result<Option<NotionPage>> {
        let conn = self.reader();
        get_page(&conn, workspace_id, page_id)
    }

    pub fn list_databases(&self, workspace_id: &str) -> Result<Vec<NotionDatabase>> {
        let conn = self.reader();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, description, schema_json,
                    url, archived, is_favorite, last_edited_time, synced_at
             FROM databases WHERE workspace_id = ?1 AND archived = 0
             ORDER BY is_favorite DESC, last_edited_time DESC, title ASC",
        )?;
        let rows = stmt.query_map(params![workspace_id], map_database_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_database(&self, workspace_id: &str, database_id: &str) -> Result<Option<NotionDatabase>> {
        let conn = self.reader();
        conn.query_row(
            "SELECT id, workspace_id, parent_type, parent_id, title, icon, description, schema_json,
                    url, archived, is_favorite, last_edited_time, synced_at
             FROM databases WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, database_id],
            map_database_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn load_page_blocks(&self, workspace_id: &str, page_id: &str) -> Result<Vec<NotionBlock>> {
        let conn = self.reader();
        load_page_blocks(&conn, workspace_id, page_id)
    }

    pub(crate) fn load_cached_children(
        &self,
        workspace_id: &str,
        parent_id: &str,
    ) -> Result<Option<Vec<NotionBlock>>> {
        let mut conn = self.reader();
        let tx = conn.transaction()?;
        let (blocks, known_empty) = match cached_child_parent(&tx, workspace_id, parent_id)? {
            Some((page_id, has_children)) => {
                let mut blocks = load_blocks_under(&tx, workspace_id, &page_id, Some(parent_id))?;
                // Match the standalone online tree, whose root is the requested block.
                set_tree_page_id(&mut blocks, parent_id);
                (blocks, !has_children)
            }
            None => (load_page_blocks(&tx, workspace_id, parent_id)?, false),
        };
        if known_empty || !blocks.is_empty() {
            return Ok(Some(blocks));
        }
        let fetched = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM block_children_fetched
             WHERE workspace_id = ?1 AND parent_id = ?2)",
            params![workspace_id, parent_id],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(fetched.then_some(blocks))
    }

    pub fn load_page_comments(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<Vec<NotionComment>> {
        let conn = self.reader();
        load_page_comments(&conn, workspace_id, page_id)
    }

    pub(crate) fn load_cached_comments(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<CachedComments> {
        let conn = self.reader();
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT comments_fetched, comments_error FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, page_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(CachedComments::NotFetched),
            Some((_, Some(err))) => Ok(CachedComments::Failed(err)),
            Some((1, None)) => Ok(CachedComments::Complete(load_page_comments(
                &conn,
                workspace_id,
                page_id,
            )?)),
            Some((2, None)) => Ok(CachedComments::Partial(load_page_comments(
                &conn,
                workspace_id,
                page_id,
            )?)),
            Some((_, None)) => Ok(CachedComments::NotFetched),
        }
    }

    /// True when the cache holds only the first part of this page's comments.
    pub(crate) fn comments_partial(&self, workspace_id: &str, page_id: &str) -> Result<bool> {
        let conn = self.reader();
        let fetched: Option<i64> = conn
            .query_row(
                "SELECT comments_fetched FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, page_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(fetched == Some(2))
    }

    pub fn load_page_detail(
        &self,
        workspace_id: &str,
        page_id: &str,
    ) -> Result<Option<NotionPageDetail>> {
        let Some(page) = self.get_page(workspace_id, page_id)? else {
            return Ok(None);
        };
        let blocks = self.load_page_blocks(workspace_id, page_id)?;
        let comments = self.load_page_comments(workspace_id, page_id)?;
        let comments_error = self.reader().query_row(
            "SELECT comments_error FROM pages WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, page_id],
            |row| row.get(0),
        ).optional()?.flatten();
        let markdown = blocks_to_markdown(&blocks);
        let media = collect_media_refs(&blocks);
        Ok(Some(NotionPageDetail {
            page,
            blocks,
            comments,
            comments_error,
            markdown,
            media,
        }))
    }

    /// Full-text search across `pages_fts` (title + plain_text + properties_summary) with
    /// automatic Korean / CJK / short `<3`-char `LIKE` fallback.
    pub fn search_local(
        &self,
        workspace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NotionSearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let lim = limit.clamp(1, 200) as i64;
        let conn = self.reader();

        let char_len = trimmed.chars().count();
        let has_short_token = trimmed
            .split_whitespace()
            .any(|tok| tok.chars().count() < 3);
        let use_like_only = char_len < 3 || (self.uses_trigram && has_short_token);

        let mut hits = Vec::new();
        if !use_like_only {
            let fts_query = build_fts_query(trimmed);
            if let Ok(mut stmt) = conn.prepare(
                "SELECT f.page_id, f.workspace_id, f.item_type, f.title,
                        snippet(pages_fts, 4, '[', ']', '...', 18) AS snip
                 FROM pages_fts f
                 WHERE f.workspace_id = ?1 AND pages_fts MATCH ?2
                 ORDER BY bm25(pages_fts)
                 LIMIT ?3",
            ) {
                if let Ok(rows) = stmt.query_map(params![workspace_id, fts_query, lim], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                }) {
                    for row in rows.flatten() {
                        if let Some(hit) = hydrate_search_hit(&conn, row.0, row.1, row.2, row.3, row.4)
                        {
                            hits.push(hit);
                        }
                    }
                }
            }
        }

        if hits.is_empty() {
            // Korean <3 char & substring LIKE fallback across both pages and databases
            let like_pat = format!("%{}%", escape_like(trimmed));
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, 'page' AS item_type, title, icon, url, last_edited_time,
                        CASE
                            WHEN plain_text != '' THEN substr(plain_text, 1, 140)
                            ELSE substr(properties_summary, 1, 140)
                        END AS snip
                 FROM pages
                 WHERE workspace_id = ?1 AND archived = 0
                   AND (
                       lower(title) LIKE lower(?2) ESCAPE '\\'
                       OR lower(plain_text) LIKE lower(?2) ESCAPE '\\'
                       OR lower(properties_summary) LIKE lower(?2) ESCAPE '\\'
                   )
                 UNION ALL
                 SELECT id, workspace_id, 'database' AS item_type, title, icon, url, last_edited_time,
                        substr(description, 1, 140) AS snip
                 FROM databases
                 WHERE workspace_id = ?1 AND archived = 0
                   AND (
                       lower(title) LIKE lower(?2) ESCAPE '\\'
                       OR lower(description) LIKE lower(?2) ESCAPE '\\'
                   )
                 ORDER BY last_edited_time DESC
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![workspace_id, like_pat, lim], |r| {
                Ok(NotionSearchHit {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    item_type: r.get(2)?,
                    title: r.get(3)?,
                    icon: r.get(4)?,
                    url: r.get(5)?,
                    last_edited_time: r.get(6)?,
                    snippet: r.get::<_, String>(7)?.replace('\n', " "),
                })
            })?;
            for row in rows {
                hits.push(row?);
            }
        }

        Ok(hits)
    }

    pub fn set_sync_cursor(&self, scope_key: &str, cursor: Option<&str>) -> Result<()> {
        let conn = self.writer();
        conn.execute(
            "INSERT INTO sync_cursors (scope_key, cursor, last_synced_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key) DO UPDATE SET
                cursor = excluded.cursor,
                last_synced_at = excluded.last_synced_at",
            params![scope_key, cursor, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get_sync_cursor(&self, scope_key: &str) -> Option<String> {
        let conn = self.reader();
        conn.query_row(
            "SELECT cursor FROM sync_cursors WHERE scope_key = ?1",
            params![scope_key],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
    }

    pub fn last_synced_at(&self, scope_key: &str) -> Option<String> {
        let conn = self.reader();
        conn.query_row(
            "SELECT last_synced_at FROM sync_cursors WHERE scope_key = ?1",
            params![scope_key],
            |r| r.get(0),
        )
        .ok()
    }

    /// Persists resolved `block_id -> owner_page_id` mappings (for pages whose `parent_type == "block_id"`).
    pub fn upsert_block_owners(
        &self,
        workspace_id: &str,
        entries: &[(String, String)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        for (block_id, owner_page_id) in entries {
            if block_id.trim().is_empty() || owner_page_id.trim().is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO block_owners (workspace_id, block_id, owner_page_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(workspace_id, block_id) DO UPDATE SET
                    owner_page_id = excluded.owner_page_id,
                    updated_at = excluded.updated_at",
                params![workspace_id, block_id, owner_page_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads local parent resolution hints for sidebar page classification and ordering:
    /// - `block_owner_pages`: maps `block_id -> owner_page_id` (from both `block_owners` and `blocks`,
    ///   including `child_page` block IDs -> their container page ID)
    /// - `child_page_positions`: maps `child_page_id -> position` on its parent page.
    pub fn load_page_parent_hints(
        &self,
        workspace_id: &str,
    ) -> Result<(HashMap<String, String>, HashMap<String, i64>)> {
        let conn = self.reader();
        let mut block_owner_pages: HashMap<String, String> = HashMap::new();
        let mut child_page_positions: HashMap<String, i64> = HashMap::new();

        if let Ok(mut stmt) = conn.prepare(
            "SELECT block_id, owner_page_id FROM block_owners WHERE workspace_id = ?1",
        ) {
            if let Ok(rows) = stmt.query_map(params![workspace_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                for (block_id, owner_page_id) in rows.flatten() {
                    block_owner_pages.insert(block_id, owner_page_id);
                }
            }
        }

        let mut stmt = conn.prepare(
            "SELECT id, page_id, block_type, position
             FROM blocks
             WHERE workspace_id = ?1",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (id, page_id, block_type, position) = row?;
            block_owner_pages.insert(id.clone(), page_id);
            if block_type == "child_page" {
                child_page_positions.insert(id, position);
            }
        }

        Ok((block_owner_pages, child_page_positions))
    }
}

fn get_page(conn: &Connection, workspace_id: &str, page_id: &str) -> Result<Option<NotionPage>> {
    conn.query_row(
        "SELECT id, workspace_id, parent_type, parent_id, title, icon, cover_url, url,
                archived, is_favorite, properties_json, plain_text, created_time, last_edited_time, synced_at
         FROM pages WHERE workspace_id = ?1 AND id = ?2",
        params![workspace_id, page_id],
        map_page_row,
    )
    .optional()
    .map_err(Into::into)
}

fn detail_sync_marker(conn: &Connection, workspace_id: &str, page_id: &str) -> Result<Option<String>> {
    let ts: Option<String> = conn
        .query_row(
            "SELECT detail_synced_edited_time FROM pages WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, page_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .filter(|s| !s.trim().is_empty());
    Ok(ts)
}

fn page_detail_matches(conn: &Connection, detail: &NotionPageDetail) -> Result<bool> {
    let page = &detail.page;
    let workspace_id = &page.workspace_id;
    let page_id = &page.id;
    let marker = detail_sync_marker(conn, workspace_id, page_id)?;
    if marker.is_none() || marker != page.last_edited_time {
        return Ok(false);
    }
    let Some(cached) = get_page(conn, workspace_id, page_id)? else {
        return Ok(false);
    };
    if !crate::sync::page_semantically_equal(&cached, page)
        || (!cached.is_favorite && page.is_favorite)
        || load_page_blocks(conn, workspace_id, page_id)? != detail.blocks
        || load_page_comments(conn, workspace_id, page_id)? != detail.comments
    {
        return Ok(false);
    }
    if page.archived {
        return Ok(true);
    }
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pages_fts
         WHERE workspace_id = ?1 AND page_id = ?2 AND item_type = 'page')",
        params![workspace_id, page_id],
        |row| row.get(0),
    )?)
}

fn load_page_blocks(conn: &Connection, workspace_id: &str, page_id: &str) -> Result<Vec<NotionBlock>> {
    load_blocks_under(conn, workspace_id, page_id, None)
}

fn load_blocks_under(
    conn: &Connection,
    workspace_id: &str,
    page_id: &str,
    parent_id: Option<&str>,
) -> Result<Vec<NotionBlock>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_block_id, content_json
         FROM blocks
         WHERE workspace_id = ?1 AND page_id = ?2
         ORDER BY position ASC",
    )?;
    let flat: Vec<(String, Option<String>, NotionBlock)> = stmt
        .query_map(params![workspace_id, page_id], |r| {
            let id: String = r.get(0)?;
            let parent_id: Option<String> = r.get(1)?;
            let json_str: String = r.get(2)?;
            Ok((id, parent_id, json_str))
        })?
        .filter_map(|res| res.ok())
        .filter_map(|(id, parent_id, json_str)| {
            let mut blk = serde_json::from_str::<NotionBlock>(&json_str).ok()?;
            blk.children.clear();
            Some((id, parent_id, blk))
        })
        .collect();

    Ok(rebuild_block_tree(flat, parent_id.map(str::to_owned)))
}

fn cached_child_parent(
    conn: &Connection,
    workspace_id: &str,
    parent_id: &str,
) -> Result<Option<(String, bool)>> {
    Ok(conn.query_row(
        "SELECT page_id, has_children FROM blocks
         WHERE workspace_id = ?1 AND id = ?2
           AND block_type NOT IN ('child_page', 'child_database')
           AND NOT EXISTS (SELECT 1 FROM pages WHERE workspace_id = ?1 AND id = ?2)",
        params![workspace_id, parent_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?)
}

fn set_tree_page_id(blocks: &mut [NotionBlock], page_id: &str) {
    for block in blocks {
        block.page_id = page_id.to_owned();
        set_tree_page_id(&mut block.children, page_id);
    }
}

fn load_page_comments(
    conn: &Connection,
    workspace_id: &str,
    page_id: &str,
) -> Result<Vec<NotionComment>> {
    let mut stmt = conn.prepare(
        "SELECT id, page_id, discussion_id, author, body, rich_text_json, created_time
         FROM comments
         WHERE workspace_id = ?1 AND page_id = ?2
         ORDER BY created_time ASC",
    )?;
    let rows = stmt.query_map(params![workspace_id, page_id], |r| {
        let rt_json: String = r.get(5)?;
        let rich_text = serde_json::from_str(&rt_json).unwrap_or_default();
        Ok(NotionComment {
            id: r.get(0)?,
            page_id: r.get(1)?,
            discussion_id: r.get(2)?,
            author: r.get(3)?,
            body: r.get(4)?,
            rich_text,
            created_time: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn upsert_page_in_tx(tx: &Connection, page: &NotionPage) -> Result<()> {
    let props_json = serde_json::to_string(&page.properties).unwrap_or_else(|_| "{}".into());
    let props_summary = page.properties_summary();

    // Preserve local `is_favorite` and non-empty `plain_text` if incoming sync only had page metadata
    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT is_favorite, plain_text FROM pages WHERE workspace_id = ?1 AND id = ?2",
            params![page.workspace_id, page.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let is_fav = page.is_favorite || existing.as_ref().is_some_and(|(f, _)| *f != 0);
    let effective_plain = if page.plain_text.trim().is_empty() {
        existing.map(|(_, p)| p).unwrap_or_default()
    } else {
        page.plain_text.clone()
    };

    tx.execute(
        "INSERT INTO pages (
            id, workspace_id, parent_type, parent_id, title, icon, cover_url, url,
            archived, is_favorite, properties_json, properties_summary, plain_text,
            created_time, last_edited_time, synced_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(workspace_id, id) DO UPDATE SET
            parent_type = excluded.parent_type,
            parent_id = excluded.parent_id,
            title = excluded.title,
            icon = excluded.icon,
            cover_url = excluded.cover_url,
            url = excluded.url,
            archived = excluded.archived,
            is_favorite = excluded.is_favorite,
            properties_json = excluded.properties_json,
            properties_summary = excluded.properties_summary,
            plain_text = excluded.plain_text,
            created_time = COALESCE(excluded.created_time, pages.created_time),
            last_edited_time = excluded.last_edited_time,
            synced_at = excluded.synced_at",
        params![
            page.id,
            page.workspace_id,
            page.parent_type,
            page.parent_id,
            page.title,
            page.icon,
            page.cover_url,
            page.url,
            i64::from(page.archived),
            i64::from(is_fav),
            props_json,
            props_summary,
            effective_plain,
            page.created_time,
            page.last_edited_time,
            page.synced_at
                .clone()
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        ],
    )?;

    if page.archived {
        tx.execute(
            "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
            params![page.workspace_id, page.id],
        )?;
    } else {
        refresh_fts_row(
            tx,
            &page.id,
            &page.workspace_id,
            "page",
            &page.title,
            &effective_plain,
            &props_summary,
        )?;
    }
    Ok(())
}

fn upsert_database_in_tx(tx: &Connection, db: &NotionDatabase) -> Result<()> {
    let schema_json = serde_json::to_string(&db.schema).unwrap_or_else(|_| "{}".into());
    let existing_fav: i64 = tx
        .query_row(
            "SELECT is_favorite FROM databases WHERE workspace_id = ?1 AND id = ?2",
            params![db.workspace_id, db.id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let is_fav = db.is_favorite || existing_fav != 0;

    tx.execute(
        "INSERT INTO databases (
            id, workspace_id, parent_type, parent_id, title, icon, description, schema_json,
            url, archived, is_favorite, last_edited_time, synced_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(workspace_id, id) DO UPDATE SET
            parent_type = excluded.parent_type,
            parent_id = excluded.parent_id,
            title = excluded.title,
            icon = excluded.icon,
            description = excluded.description,
            schema_json = excluded.schema_json,
            url = excluded.url,
            archived = excluded.archived,
            is_favorite = excluded.is_favorite,
            last_edited_time = excluded.last_edited_time,
            synced_at = excluded.synced_at",
        params![
            db.id,
            db.workspace_id,
            db.parent_type,
            db.parent_id,
            db.title,
            db.icon,
            db.description,
            schema_json,
            db.url,
            i64::from(db.archived),
            i64::from(is_fav),
            db.last_edited_time,
            db.synced_at
                .clone()
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        ],
    )?;

    if db.archived {
        tx.execute(
            "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
            params![db.workspace_id, db.id],
        )?;
    } else {
        refresh_fts_row(
            tx,
            &db.id,
            &db.workspace_id,
            "database",
            &db.title,
            &db.description,
            "",
        )?;
    }
    Ok(())
}

fn replace_page_blocks_in_tx(
    tx: &Connection,
    workspace_id: &str,
    page_id: &str,
    blocks: &[NotionBlock],
) -> Result<()> {
    tx.execute(
        "DELETE FROM blocks WHERE workspace_id = ?1 AND page_id = ?2",
        params![workspace_id, page_id],
    )?;
    let mut position = 0i64;
    insert_blocks_recursive(tx, workspace_id, page_id, None, blocks, &mut position)?;
    mark_children_fetched(tx, workspace_id, page_id)?;
    update_page_plain_text_in_tx(tx, workspace_id, page_id, &blocks_to_plain_text(blocks))
}

fn replace_nested_children(
    tx: &Connection,
    workspace_id: &str,
    owner_id: &str,
    parent_id: &str,
    blocks: &[NotionBlock],
) -> Result<()> {
    // Store a nested read under its owning page. The block ID is unique per workspace.
    tx.execute(
        "WITH RECURSIVE descendants(id) AS (
             SELECT id FROM blocks WHERE workspace_id = ?1 AND parent_block_id = ?2
             UNION
             SELECT b.id FROM blocks b JOIN descendants d ON b.parent_block_id = d.id
             WHERE b.workspace_id = ?1
         )
         DELETE FROM blocks WHERE workspace_id = ?1
           AND (page_id = ?2 OR id IN (SELECT id FROM descendants))",
        params![workspace_id, parent_id],
    )?;
    let mut position = 0;
    insert_blocks_recursive(tx, workspace_id, owner_id, Some(parent_id), blocks, &mut position)?;
    tx.execute(
        "UPDATE blocks SET has_children = ?1, content_json = json_set(content_json, '$.has_children', json(?2))
         WHERE workspace_id = ?3 AND id = ?4",
        params![!blocks.is_empty(), if blocks.is_empty() { "false" } else { "true" }, workspace_id, parent_id],
    )?;
    // Page detail may have changed remotely in addition to this subtree.
    tx.execute(
        "UPDATE pages SET detail_synced_edited_time = NULL, body_synced_edited_time = NULL WHERE workspace_id = ?1 AND id = ?2",
        params![workspace_id, owner_id],
    )?;
    mark_children_fetched(tx, workspace_id, parent_id)?;
    recompute_page_plain_text_in_tx(tx, workspace_id, owner_id)
}

fn mark_children_fetched(tx: &Connection, workspace_id: &str, parent_id: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO block_children_fetched (workspace_id, parent_id) VALUES (?1, ?2)",
        params![workspace_id, parent_id],
    )?;
    Ok(())
}

fn replace_page_comments_in_tx(
    tx: &Connection,
    workspace_id: &str,
    page_id: &str,
    comments: &[NotionComment],
) -> Result<()> {
    tx.execute(
        "DELETE FROM comments WHERE workspace_id = ?1 AND page_id = ?2",
        params![workspace_id, page_id],
    )?;
    for c in comments {
        let rt_json = serde_json::to_string(&c.rich_text).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT OR REPLACE INTO comments
             (id, workspace_id, page_id, discussion_id, author, body, rich_text_json, created_time)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                c.id,
                workspace_id,
                page_id,
                c.discussion_id,
                c.author,
                c.body,
                rt_json,
                c.created_time
            ],
        )?;
    }
    Ok(())
}

fn delete_block_subtree_in_tx(tx: &Connection, workspace_id: &str, block_id: &str) -> Result<()> {
    tx.execute(
        "WITH RECURSIVE sub(id) AS (
             SELECT ?2
             UNION ALL
             SELECT b.id FROM blocks b
             JOIN sub s ON b.parent_block_id = s.id
             WHERE b.workspace_id = ?1
         )
         DELETE FROM blocks WHERE workspace_id = ?1 AND id IN (SELECT id FROM sub)",
        params![workspace_id, block_id],
    )?;
    Ok(())
}

fn recompute_page_plain_text_in_tx(
    tx: &Connection,
    workspace_id: &str,
    page_id: &str,
) -> Result<()> {
    let blocks = load_page_blocks(tx, workspace_id, page_id)?;
    let plain = blocks_to_plain_text(&blocks);
    update_page_plain_text_in_tx(tx, workspace_id, page_id, &plain)
}

fn update_page_plain_text_in_tx(
    tx: &Connection,
    workspace_id: &str,
    page_id: &str,
    plain: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE pages SET plain_text = ?1, synced_at = ?2 WHERE workspace_id = ?3 AND id = ?4",
        params![plain, chrono::Utc::now().to_rfc3339(), workspace_id, page_id],
    )?;
    let row: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT title, properties_summary, archived FROM pages WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id, page_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((title, props_summary, archived)) = row {
        if archived == 0 {
            refresh_fts_row(tx, page_id, workspace_id, "page", &title, plain, &props_summary)?;
        }
    }
    Ok(())
}

fn insert_blocks_recursive(
    tx: &Connection,
    workspace_id: &str,
    page_id: &str,
    parent_block_id: Option<&str>,
    blocks: &[NotionBlock],
    position: &mut i64,
) -> Result<()> {
    for blk in blocks {
        let mut flat = blk.clone();
        flat.children.clear();
        flat.page_id = page_id.to_owned();
        if let Some(parent) = parent_block_id {
            flat.parent_block_id = Some(parent.to_owned());
        }
        let content_json = serde_json::to_string(&flat).unwrap_or_else(|_| "{}".into());
        let plain = flat.plain_text();
        tx.execute(
            "INSERT OR REPLACE INTO blocks
             (id, page_id, parent_block_id, workspace_id, position, block_type, content_json, plain_text, has_children)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blk.id,
                page_id,
                parent_block_id,
                workspace_id,
                *position,
                blk.block_type,
                content_json,
                plain,
                i64::from(blk.has_children || !blk.children.is_empty()),
            ],
        )?;
        *position += 1;
        if !blk.children.is_empty() {
            insert_blocks_recursive(tx, workspace_id, page_id, Some(&blk.id), &blk.children, position)?;
            mark_children_fetched(tx, workspace_id, &blk.id)?;
        } else if blk.has_children && !matches!(blk.block_type.as_str(), "child_page" | "child_database") {
            // A depth-limited snapshot indicates children that it did not cache.
            tx.execute(
                "DELETE FROM block_children_fetched WHERE workspace_id = ?1 AND parent_id = ?2",
                params![workspace_id, blk.id],
            )?;
        }
    }
    Ok(())
}

fn rebuild_block_tree(flat: Vec<(String, Option<String>, NotionBlock)>, root: Option<String>) -> Vec<NotionBlock> {
    let mut children_map: HashMap<Option<String>, Vec<NotionBlock>> = HashMap::new();
    for (_id, parent_id, blk) in flat {
        children_map.entry(parent_id).or_default().push(blk);
    }
    fn attach(
        parent: Option<String>,
        map: &mut HashMap<Option<String>, Vec<NotionBlock>>,
    ) -> Vec<NotionBlock> {
        let Some(mut list) = map.remove(&parent) else {
            return Vec::new();
        };
        for item in &mut list {
            item.children = attach(Some(item.id.clone()), map);
        }
        list
    }
    attach(root, &mut children_map)
}

fn refresh_fts_row(
    tx: &Connection,
    page_id: &str,
    workspace_id: &str,
    item_type: &str,
    title: &str,
    plain_text: &str,
    properties_summary: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM pages_fts WHERE workspace_id = ?1 AND page_id = ?2",
        params![workspace_id, page_id],
    )?;
    tx.execute(
        "INSERT INTO pages_fts (page_id, workspace_id, item_type, title, plain_text, properties_summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![page_id, workspace_id, item_type, title, plain_text, properties_summary],
    )?;
    Ok(())
}

fn hydrate_search_hit(
    conn: &Connection,
    id: String,
    workspace_id: String,
    item_type: String,
    title: String,
    snippet: String,
) -> Option<NotionSearchHit> {
    let (icon, url, last_edited_time): (Option<String>, String, Option<String>) =
        if item_type == "database" {
            conn.query_row(
                "SELECT icon, url, last_edited_time FROM databases WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()?
        } else {
            conn.query_row(
                "SELECT icon, url, last_edited_time FROM pages WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()?
        };
    Some(NotionSearchHit {
        id,
        workspace_id,
        item_type,
        title,
        icon,
        snippet: snippet.replace('\n', " "),
        url,
        last_edited_time,
    })
}

fn map_page_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NotionPage> {
    let props_json: String = r.get(10)?;
    let properties: BTreeMap<String, NotionPropertyValue> =
        serde_json::from_str(&props_json).unwrap_or_default();
    Ok(NotionPage {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        parent_type: r.get(2)?,
        parent_id: r.get(3)?,
        title: r.get(4)?,
        icon: r.get(5)?,
        cover_url: r.get(6)?,
        url: r.get(7)?,
        archived: r.get::<_, i64>(8)? != 0,
        is_favorite: r.get::<_, i64>(9)? != 0,
        properties,
        plain_text: r.get(11)?,
        created_time: r.get(12)?,
        last_edited_time: r.get(13)?,
        synced_at: r.get(14)?,
    })
}

fn map_database_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NotionDatabase> {
    let schema_json: String = r.get(7)?;
    let schema: BTreeMap<String, NotionPropertySchema> =
        serde_json::from_str(&schema_json).unwrap_or_default();
    Ok(NotionDatabase {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        parent_type: r.get(2)?,
        parent_id: r.get(3)?,
        title: r.get(4)?,
        icon: r.get(5)?,
        description: r.get(6)?,
        schema,
        url: r.get(8)?,
        archived: r.get::<_, i64>(9)? != 0,
        is_favorite: r.get::<_, i64>(10)? != 0,
        last_edited_time: r.get(11)?,
        synced_at: r.get(12)?,
    })
}

fn build_fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NotionBlockKind, NotionColor, RichTextSpan, SelectOption};

    #[test]
    fn unchanged_detail_keeps_the_index_and_detects_missing_or_changed_rows() {
        let storage = Storage::open_in_memory().unwrap();
        let mut detail = NotionPageDetail {
            page: NotionPage {
                id: "poll".into(),
                workspace_id: "ws".into(),
                last_edited_time: Some("2026-11-21T00:00:00Z".into()),
                ..Default::default()
            },
            blocks: vec![NotionBlock {
                id: "parent".into(),
                page_id: "poll".into(),
                has_children: true,
                children: vec![NotionBlock {
                    id: "image".into(),
                    page_id: "poll".into(),
                    parent_block_id: Some("parent".into()),
                    block_type: "image".into(),
                    kind: NotionBlockKind::Image {
                        url: "https://example.com/first.png".into(),
                        caption: Vec::new(),
                        is_hosted: true,
                    },
                    ..Default::default()
                }],
                ..Default::default()
            }],
            comments: vec![NotionComment {
                id: "comment".into(),
                page_id: "poll".into(),
                body: "Before".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        detail.page.plain_text = blocks_to_plain_text(&detail.blocks);
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());

        storage.writer().execute(
            "UPDATE pages SET synced_at = 'previous' WHERE id = 'poll'",
            [],
        ).unwrap();
        assert!(!storage.save_page_detail_if_changed(&detail).unwrap());
        let saved = storage.get_page("ws", "poll").unwrap().unwrap();
        assert_ne!(saved.synced_at.as_deref(), Some("previous"));
        assert_eq!(
            storage.get_page_detail_synced_edited_time("ws", "poll").unwrap(),
            detail.page.last_edited_time,
        );

        let NotionBlockKind::Image { url, .. } = &mut detail.blocks[0].children[0].kind else {
            panic!("expected nested image");
        };
        *url = "https://example.com/renewed.png".into();
        detail.page.plain_text = blocks_to_plain_text(&detail.blocks);
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());
        detail.comments[0].body = "Edited comment".into();
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());
        let saved = storage.load_page_detail("ws", "poll").unwrap().unwrap();
        assert_eq!(saved.blocks, detail.blocks);
        assert_eq!(saved.comments, detail.comments);

        storage.writer().execute("DELETE FROM blocks WHERE id = 'image'", []).unwrap();
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());
        storage.writer().execute("DELETE FROM pages_fts WHERE page_id = 'poll'", []).unwrap();
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());
        storage.invalidate_page_detail("ws", "poll").unwrap();
        assert!(storage.save_page_detail_if_changed(&detail).unwrap());
    }

    #[test]
    fn test_sqlite_fts5_and_korean_short_like_fallback() {
        let storage = Storage::open_in_memory().unwrap();
        let mut props = BTreeMap::new();
        props.insert(
            "Status".into(),
            NotionPropertyValue::Status(Some(SelectOption {
                id: None,
                name: "In Progress".into(),
                color: NotionColor::Blue,
            })),
        );

        let page1 = NotionPage {
            id: "p-1".into(),
            workspace_id: "ws-1".into(),
            parent_type: "workspace".into(),
            parent_id: None,
            title: "Q4 Architecture Review".into(),
            icon: Some("🏛".into()),
            url: "https://notion.so/p-1".into(),
            properties: props,
            plain_text: "Distributed SQLite WAL and FTS5 design".into(),
            last_edited_time: Some("2026-11-20T01:00:00Z".into()),
            ..Default::default()
        };
        let page2 = NotionPage {
            id: "p-2".into(),
            workspace_id: "ws-1".into(),
            parent_type: "workspace".into(),
            parent_id: None,
            title: "주간 회의록".into(),
            icon: Some("📝".into()),
            url: "https://notion.so/p-2".into(),
            plain_text: "기획 논의 및 일정 정리".into(),
            last_edited_time: Some("2026-11-20T02:00:00Z".into()),
            ..Default::default()
        };

        storage.upsert_pages(&[page1, page2]).unwrap();

        // Standard 3+ char FTS5 match
        let hits = storage.search_local("ws-1", "Architecture", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "p-1");

        // Property summary search
        let prop_hits = storage.search_local("ws-1", "In Progress", 10).unwrap();
        assert_eq!(prop_hits.len(), 1);
        assert_eq!(prop_hits[0].id, "p-1");

        // Korean 2-char (<3) LIKE fallback
        let ko2 = storage.search_local("ws-1", "회의", 10).unwrap();
        assert_eq!(ko2.len(), 1);
        assert_eq!(ko2[0].id, "p-2");

        // Korean 1-char (<3) LIKE fallback
        let ko1 = storage.search_local("ws-1", "기", 10).unwrap();
        assert_eq!(ko1.len(), 1);
        assert_eq!(ko1[0].id, "p-2");

        // Nested block tree roundtrip
        let toggle = NotionBlock {
            id: "b-toggle".into(),
            page_id: "p-1".into(),
            block_type: "toggle".into(),
            has_children: true,
            kind: NotionBlockKind::Toggle {
                rich_text: vec![RichTextSpan::plain("Deep Dive")],
                color: NotionColor::Default,
            },
            children: vec![NotionBlock {
                id: "b-child".into(),
                page_id: "p-1".into(),
                parent_block_id: Some("b-toggle".into()),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Nested detail")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        storage.replace_page_blocks("ws-1", "p-1", &[toggle]).unwrap();
        let loaded = storage.load_page_blocks("ws-1", "p-1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].children.len(), 1);
        assert_eq!(loaded[0].children[0].plain_text(), "Nested detail");
    }

    #[test]
    fn offline_children_keep_verified_empty_and_nested_page_cache() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.load_cached_children("ws", "empty").unwrap().is_none());
        storage.replace_page_blocks("ws", "empty", &[]).unwrap();
        assert_eq!(storage.load_cached_children("ws", "empty").unwrap(), Some(Vec::new()));
        assert!(storage.load_cached_children("other", "empty").unwrap().is_none());

        let block = |id: &str| NotionBlock { id: id.into(), block_type: "paragraph".into(), ..Default::default() };
        let mut parent = block("parent");
        parent.has_children = true;
        parent.children = vec![block("old-child")];
        storage.replace_page_blocks("ws", "page", &[parent, block("sibling")]).unwrap();
        let cached = storage.load_cached_children("ws", "parent").unwrap().unwrap();
        assert_eq!(cached[0].id, "old-child");
        assert_eq!(cached[0].page_id, "parent");

        storage.replace_page_blocks("ws", "parent", &[block("new-child")]).unwrap();
        let tree = storage.load_page_blocks("ws", "page").unwrap();
        assert_eq!(tree[0].id, "parent");
        assert_eq!(tree[0].children[0].id, "new-child");
        assert_eq!(tree[0].children[0].page_id, "page");
        assert_eq!(tree[1].id, "sibling");

        let mut depth_limited = block("parent");
        depth_limited.has_children = true;
        storage.replace_page_blocks("ws", "page", &[depth_limited, block("sibling")]).unwrap();
        assert!(storage.load_cached_children("ws", "parent").unwrap().is_none());

        storage.replace_page_blocks("ws", "parent", &[]).unwrap();
        assert_eq!(storage.load_cached_children("ws", "parent").unwrap(), Some(Vec::new()));
        let tree = storage.load_page_blocks("ws", "page").unwrap();
        assert!(!tree[0].has_children);
        assert!(tree[0].children.is_empty());
        assert_eq!(tree[1].id, "sibling");
    }

    #[test]
    fn test_migration_v13_preserves_cached_data_and_leaves_detail_synced_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let db_file = tmp.path().join("legacy_v13.db");
        {
            let conn = rusqlite::Connection::open(&db_file).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT NOT NULL, icon TEXT, bot_id TEXT, bot_name TEXT, auth_mode TEXT NOT NULL DEFAULT 'integration', updated_at TEXT NOT NULL);
                CREATE TABLE pages (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, parent_type TEXT NOT NULL DEFAULT 'workspace',
                    parent_id TEXT, title TEXT NOT NULL, icon TEXT, cover_url TEXT, url TEXT NOT NULL DEFAULT '',
                    archived INTEGER NOT NULL DEFAULT 0, is_favorite INTEGER NOT NULL DEFAULT 0,
                    properties_json TEXT NOT NULL DEFAULT '{}', properties_summary TEXT NOT NULL DEFAULT '',
                    plain_text TEXT NOT NULL DEFAULT '', created_time TEXT, last_edited_time TEXT, synced_at TEXT,
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE databases (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, parent_type TEXT NOT NULL DEFAULT 'workspace',
                    parent_id TEXT, title TEXT NOT NULL, icon TEXT, description TEXT NOT NULL DEFAULT '',
                    schema_json TEXT NOT NULL DEFAULT '{}', url TEXT NOT NULL DEFAULT '',
                    archived INTEGER NOT NULL DEFAULT 0, is_favorite INTEGER NOT NULL DEFAULT 0,
                    last_edited_time TEXT, synced_at TEXT, PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE blocks (
                    id TEXT NOT NULL, page_id TEXT NOT NULL, parent_block_id TEXT, workspace_id TEXT NOT NULL,
                    position INTEGER NOT NULL DEFAULT 0, block_type TEXT NOT NULL, content_json TEXT NOT NULL,
                    plain_text TEXT NOT NULL DEFAULT '', has_children INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE comments (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, page_id TEXT NOT NULL,
                    discussion_id TEXT NOT NULL DEFAULT '', author TEXT NOT NULL, body TEXT NOT NULL,
                    rich_text_json TEXT NOT NULL DEFAULT '[]', created_time TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE sync_cursors (scope_key TEXT PRIMARY KEY, cursor TEXT, last_synced_at TEXT NOT NULL);
                CREATE TABLE block_owners (workspace_id TEXT NOT NULL, block_id TEXT NOT NULL, owner_page_id TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (workspace_id, block_id));
                CREATE VIRTUAL TABLE pages_fts USING fts5(page_id UNINDEXED, workspace_id UNINDEXED, item_type UNINDEXED, title, plain_text, properties_summary, tokenize='unicode61');
                INSERT INTO pages (id, workspace_id, title, plain_text, last_edited_time)
                VALUES ('p-legacy', 'ws-1', 'Legacy Spec', 'Cached body from v13', '2026-11-20T01:00:00.000Z');
                INSERT INTO comments (id, workspace_id, page_id, author, body, created_time)
                VALUES ('c-1', 'ws-1', 'p-legacy', 'Alice', 'Legacy comment', '2026-11-20T01:00:00.000Z');
                INSERT INTO pages_fts (page_id, workspace_id, item_type, title, plain_text, properties_summary)
                VALUES ('p-legacy', 'ws-1', 'page', 'Legacy Spec', 'Cached body from v13', '');
                PRAGMA user_version = 12;
                ",
            )
            .unwrap();
            let blk = NotionBlock {
                id: "b-legacy".into(),
                page_id: "p-legacy".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Cached body from v13")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            };
            conn.execute(
                "INSERT INTO blocks (id, page_id, workspace_id, position, block_type, content_json, plain_text)
                 VALUES (?1, ?2, ?3, 0, 'paragraph', ?4, ?5)",
                params![
                    blk.id,
                    blk.page_id,
                    "ws-1",
                    serde_json::to_string(&blk).unwrap(),
                    "Cached body from v13",
                ],
            )
            .unwrap();
        }

        let storage = Storage::open(&db_file).unwrap();
        // Migrated DB must keep detail_synced_edited_time empty so first online open refreshes once,
        // while offline reads still return the cached blocks, comments, and FTS search results.
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-legacy")
                .unwrap(),
            None
        );
        let offline = storage.load_page_detail("ws-1", "p-legacy").unwrap().unwrap();
        assert_eq!(offline.page.title, "Legacy Spec");
        assert_eq!(offline.blocks.len(), 1);
        assert_eq!(offline.blocks[0].plain_text(), "Cached body from v13");
        assert_eq!(offline.comments.len(), 1);
        assert_eq!(offline.comments[0].body, "Legacy comment");
        let hits = storage.search_local("ws-1", "Legacy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "p-legacy");
    }

    #[test]
    fn test_upsert_pages_preserves_detail_synced_and_block_write_reconciliation() {
        let storage = Storage::open_in_memory().unwrap();
        let page_v2 = NotionPage {
            id: "p-1".into(),
            workspace_id: "ws-1".into(),
            parent_type: "workspace".into(),
            title: "Spec".into(),
            last_edited_time: Some("2026-11-20T02:00:00.000Z".into()),
            ..Default::default()
        };
        let todo_blk = NotionBlock {
            id: "b-todo".into(),
            page_id: "p-1".into(),
            block_type: "to_do".into(),
            kind: NotionBlockKind::ToDo {
                rich_text: vec![RichTextSpan::plain("Ship patch")],
                checked: false,
                color: NotionColor::Default,
            },
            ..Default::default()
        };
        let del_blk = NotionBlock {
            id: "b-del".into(),
            page_id: "p-1".into(),
            block_type: "paragraph".into(),
            has_children: true,
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain("Delete me")],
                color: NotionColor::Default,
            },
            children: vec![NotionBlock {
                id: "b-del-child".into(),
                page_id: "p-1".into(),
                parent_block_id: Some("b-del".into()),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Nested child")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let detail = NotionPageDetail {
            page: page_v2.clone(),
            blocks: vec![todo_blk.clone(), del_blk],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: Vec::new(),
        };
        storage.save_page_detail(&detail).unwrap();
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-1")
                .unwrap()
                .as_deref(),
            Some("2026-11-20T02:00:00.000Z")
        );

        // List sync updates `last_edited_time` to T3 without touching `detail_synced_edited_time` (T2).
        let mut page_v3 = page_v2;
        page_v3.last_edited_time = Some("2026-11-20T03:00:00.000Z".into());
        storage.upsert_pages(&[page_v3]).unwrap();
        assert_eq!(
            storage
                .get_page("ws-1", "p-1")
                .unwrap()
                .unwrap()
                .last_edited_time
                .as_deref(),
            Some("2026-11-20T03:00:00.000Z")
        );
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-1")
                .unwrap()
                .as_deref(),
            Some("2026-11-20T02:00:00.000Z")
        );

        // Apply single updated to_do block -> cached blocks + plain_text updated immediately.
        let mut checked_todo = todo_blk;
        checked_todo.kind = NotionBlockKind::ToDo {
            rich_text: vec![RichTextSpan::plain("Ship patch done")],
            checked: true,
            color: NotionColor::Default,
        };
        let applied = storage.apply_updated_block("ws-1", &checked_todo).unwrap();
        assert_eq!(applied, Some(("p-1".to_string(), false)));
        let loaded = storage.load_page_detail("ws-1", "p-1").unwrap().unwrap();
        assert!(matches!(
            loaded.blocks[0].kind,
            NotionBlockKind::ToDo { checked: true, .. }
        ));
        assert!(loaded.page.plain_text.contains("Ship patch done"));

        // Deleting parent block removes both parent and nested child from SQLite cache.
        assert_eq!(
            storage.delete_cached_block("ws-1", "b-del").unwrap(),
            Some("p-1".to_string())
        );
        let after_del = storage.load_page_detail("ws-1", "p-1").unwrap().unwrap();
        assert_eq!(after_del.blocks.len(), 1);
        assert!(!after_del.page.plain_text.contains("Delete me"));
        assert!(!after_del.page.plain_text.contains("Nested child"));

        // If updated block reports new children we didn't cache, detail marker is invalidated.
        let mut now_has_children = checked_todo;
        now_has_children.has_children = true;
        let applied_nested = storage
            .apply_updated_block("ws-1", &now_has_children)
            .unwrap();
        assert_eq!(applied_nested, Some(("p-1".to_string(), true)));
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-1")
                .unwrap(),
            None
        );
    }

    #[test]
    fn save_page_body_leaves_comment_state_alone() {
        let storage = Storage::open_in_memory().unwrap();
        let make_block = |id: &str, page_id: &str, text: &str| NotionBlock {
            id: id.into(),
            page_id: page_id.into(),
            block_type: "paragraph".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain(text)],
                color: NotionColor::Default,
            },
            ..Default::default()
        };
        let make_page = |id: &str, edited: &str| NotionPage {
            id: id.into(),
            workspace_id: "ws-1".into(),
            parent_type: "workspace".into(),
            title: id.into(),
            last_edited_time: Some(edited.into()),
            ..Default::default()
        };
        let make_comment = |id: &str, page_id: &str| NotionComment {
            id: id.into(),
            page_id: page_id.into(),
            author: "Bot".into(),
            body: id.into(),
            created_time: "2026-01-01T00:00:00.000Z".into(),
            ..Default::default()
        };

        // (a) Page previously saved via save_page_detail with 2 comments
        storage
            .save_page_detail(&NotionPageDetail {
                page: make_page("p-cached", "2026-11-20T01:00:00.000Z"),
                blocks: vec![make_block("b-old-a", "p-cached", "old a")],
                comments: vec![
                    make_comment("c-1", "p-cached"),
                    make_comment("c-2", "p-cached"),
                ],
                comments_error: None,
                ..Default::default()
            })
            .unwrap();

        // (b) Page previously saved with comments_error: Some("E")
        storage
            .save_page_detail(&NotionPageDetail {
                page: make_page("p-failed", "2026-11-20T01:00:00.000Z"),
                blocks: vec![make_block("b-old-b", "p-failed", "old b")],
                comments: Vec::new(),
                comments_error: Some("E".into()),
                ..Default::default()
            })
            .unwrap();

        let t2 = "2026-11-20T02:00:00.000Z";
        for (pid, bid, txt) in [
            ("p-cached", "b-new-a", "new a"),
            ("p-failed", "b-new-b", "new b"),
            ("p-new", "b-new-c", "new c"),
        ] {
            let p = make_page(pid, t2);
            let blks = vec![make_block(bid, pid, txt)];
            storage.save_page_body(&p, &blks).unwrap();
            assert_eq!(
                storage
                    .get_page_body_synced_edited_time("ws-1", pid)
                    .unwrap()
                    .as_deref(),
                Some(t2)
            );
            let loaded = storage.load_cached_children("ws-1", pid).unwrap().unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].id, bid);
        }

        let s_cached = storage.cached_comments_summary("ws-1", "p-cached").unwrap();
        assert_eq!(s_cached.state, "cached");
        assert_eq!(s_cached.count, Some(2));
        assert_eq!(s_cached.error, None);
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-cached")
                .unwrap()
                .as_deref(),
            Some("2026-11-20T01:00:00.000Z")
        );

        let s_failed = storage.cached_comments_summary("ws-1", "p-failed").unwrap();
        assert_eq!(s_failed.state, "failed");
        assert_eq!(s_failed.count, None);
        assert_eq!(s_failed.error.as_deref(), Some("E"));

        let s_new = storage.cached_comments_summary("ws-1", "p-new").unwrap();
        assert_eq!(s_new.state, "not_fetched");
        assert_eq!(s_new.count, None);
        assert_eq!(s_new.error, None);
    }

    #[test]
    fn migration_backfills_comments_fetched() {
        let tmp = tempfile::tempdir().unwrap();
        let db_file = tmp.path().join("legacy_v19.db");
        {
            let conn = rusqlite::Connection::open(&db_file).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT NOT NULL, icon TEXT, bot_id TEXT, bot_name TEXT, auth_mode TEXT NOT NULL DEFAULT 'integration', updated_at TEXT NOT NULL);
                CREATE TABLE pages (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, parent_type TEXT NOT NULL DEFAULT 'workspace',
                    parent_id TEXT, title TEXT NOT NULL, icon TEXT, cover_url TEXT, url TEXT NOT NULL DEFAULT '',
                    archived INTEGER NOT NULL DEFAULT 0, is_favorite INTEGER NOT NULL DEFAULT 0,
                    properties_json TEXT NOT NULL DEFAULT '{}', properties_summary TEXT NOT NULL DEFAULT '',
                    plain_text TEXT NOT NULL DEFAULT '', created_time TEXT, last_edited_time TEXT,
                    detail_synced_edited_time TEXT, comments_error TEXT, synced_at TEXT,
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE databases (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, parent_type TEXT NOT NULL DEFAULT 'workspace',
                    parent_id TEXT, title TEXT NOT NULL, icon TEXT, description TEXT NOT NULL DEFAULT '',
                    schema_json TEXT NOT NULL DEFAULT '{}', url TEXT NOT NULL DEFAULT '',
                    archived INTEGER NOT NULL DEFAULT 0, is_favorite INTEGER NOT NULL DEFAULT 0,
                    last_edited_time TEXT, synced_at TEXT, PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE blocks (
                    id TEXT NOT NULL, page_id TEXT NOT NULL, parent_block_id TEXT, workspace_id TEXT NOT NULL,
                    position INTEGER NOT NULL DEFAULT 0, block_type TEXT NOT NULL, content_json TEXT NOT NULL,
                    plain_text TEXT NOT NULL DEFAULT '', has_children INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE block_children_fetched (
                    workspace_id TEXT NOT NULL, parent_id TEXT NOT NULL,
                    PRIMARY KEY (workspace_id, parent_id)
                ) WITHOUT ROWID;
                CREATE TABLE comments (
                    id TEXT NOT NULL, workspace_id TEXT NOT NULL, page_id TEXT NOT NULL,
                    discussion_id TEXT NOT NULL DEFAULT '', author TEXT NOT NULL, body TEXT NOT NULL,
                    rich_text_json TEXT NOT NULL DEFAULT '[]', created_time TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (workspace_id, id)
                );
                CREATE TABLE sync_cursors (scope_key TEXT PRIMARY KEY, cursor TEXT, last_synced_at TEXT NOT NULL);
                CREATE TABLE block_owners (workspace_id TEXT NOT NULL, block_id TEXT NOT NULL, owner_page_id TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (workspace_id, block_id));
                CREATE VIRTUAL TABLE pages_fts USING fts5(page_id UNINDEXED, workspace_id UNINDEXED, item_type UNINDEXED, title, plain_text, properties_summary, tokenize='unicode61');
                INSERT INTO pages (id, workspace_id, title, detail_synced_edited_time) VALUES
                    ('p-3', 'ws-1', 'Three', NULL),
                    ('p-synced', 'ws-1', 'Synced Empty', '2026-11-20T01:00:00.000Z'),
                    ('p-cold', 'ws-1', 'Cold', NULL),
                    ('p-2000', 'ws-1', 'Two Thousand', '2026-11-20T01:00:00.000Z');
                INSERT INTO comments (id, workspace_id, page_id, author, body, created_time) VALUES
                    ('c-1', 'ws-1', 'p-3', 'Bot', '1', '2026-01-01T00:00:01Z'),
                    ('c-2', 'ws-1', 'p-3', 'Bot', '2', '2026-01-01T00:00:02Z'),
                    ('c-3', 'ws-1', 'p-3', 'Bot', '3', '2026-01-01T00:00:03Z');
                PRAGMA user_version = 18;
                ",
            )
            .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO comments (id, workspace_id, page_id, author, body, created_time)
                         VALUES (?1, 'ws-1', 'p-2000', 'Bot', 'x', '2026-01-01T00:00:00Z')",
                    )
                    .unwrap();
                for i in 0..2000 {
                    stmt.execute(params![format!("mc-{i}")]).unwrap();
                }
            }
            tx.commit().unwrap();
        }

        let storage = Storage::open(&db_file).unwrap();
        for pid in ["p-3", "p-synced", "p-cold", "p-2000"] {
            assert_eq!(
                storage.get_page_body_synced_edited_time("ws-1", pid).unwrap(),
                None
            );
        }

        match storage.load_cached_comments("ws-1", "p-3").unwrap() {
            CachedComments::Complete(list) => assert_eq!(list.len(), 3),
            other => panic!("expected Complete(3), got {:?}", std::mem::discriminant(&other)),
        }
        match storage.load_cached_comments("ws-1", "p-synced").unwrap() {
            CachedComments::Complete(list) => assert_eq!(list.len(), 0),
            other => panic!("expected Complete(0), got {:?}", std::mem::discriminant(&other)),
        }
        assert!(matches!(
            storage.load_cached_comments("ws-1", "p-cold").unwrap(),
            CachedComments::NotFetched
        ));
        match storage.load_cached_comments("ws-1", "p-2000").unwrap() {
            CachedComments::Partial(list) => assert_eq!(list.len(), 2000),
            other => panic!("expected Partial(2000), got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(
            storage
                .get_page_detail_synced_edited_time("ws-1", "p-2000")
                .unwrap(),
            None
        );
    }

    #[test]
    fn comments_fetched_tracks_complete_and_partial_saves() {
        let storage = Storage::open_in_memory().unwrap();
        let make_comments = |n: usize| -> Vec<NotionComment> {
            (0..n)
                .map(|i| NotionComment {
                    id: format!("c-{i}"),
                    page_id: "p-1".into(),
                    author: "Bot".into(),
                    body: format!("body-{i}"),
                    created_time: format!("2026-01-01T00:00:{:02}.{:03}Z", i % 60, i % 1000),
                    ..Default::default()
                })
                .collect()
        };
        let page = NotionPage {
            id: "p-1".into(),
            workspace_id: "ws-1".into(),
            title: "Many".into(),
            last_edited_time: Some("2026-11-20T01:00:00.000Z".into()),
            ..Default::default()
        };

        storage
            .save_page_detail(&NotionPageDetail {
                page: page.clone(),
                comments: make_comments(2001),
                ..Default::default()
            })
            .unwrap();
        assert!(storage.comments_partial("ws-1", "p-1").unwrap());

        storage
            .save_page_detail(&NotionPageDetail {
                page,
                comments: make_comments(2000),
                ..Default::default()
            })
            .unwrap();
        assert!(!storage.comments_partial("ws-1", "p-1").unwrap());

        storage
            .replace_page_comments("ws-1", "p-1", &make_comments(2100), true)
            .unwrap();
        assert!(!storage.comments_partial("ws-1", "p-1").unwrap());
        match storage.load_cached_comments("ws-1", "p-1").unwrap() {
            CachedComments::Complete(list) => assert_eq!(list.len(), 2100),
            _ => panic!("expected Complete(2100)"),
        }
    }
}
