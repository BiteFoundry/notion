# `@bitefoundry/notion` MCP Server Reference (19 Tools)

Run the standalone stdio JSON-RPC 2.0 Model Context Protocol server:

```bash
notion mcp
```

### Example MCP Client Configuration

```json
{
  "mcpServers": {
    "notion": {
      "command": "notion",
      "args": ["mcp"]
    }
  }
}
```

## Available Tools (19)

| # | Tool | Required Args | Description |
|---|---|---|---|
| 1 | `whoami` | — | Return the currently authenticated Notion user/bot and active workspace metadata. |
| 2 | `list_workspaces` | — | List all configured Notion workspace profiles and the currently active workspace. |
| 3 | `search_pages` | `query` | Search Notion pages and databases using local SQLite FTS5 (with Korean `<3` char LIKE fallback) or remote Notion Search API. |
| 4 | `get_page` | `page` | Fetch a Notion page. Compact by default (`compact: true`): returns `id`, `title`, `last_edited_time`, `markdown`, `incomplete_blocks`, `comments_state`, `omitted` counts, and a `hint` without requesting `/v1/comments`. Pass `compact: false` for properties, block tree, comments (up to 2,000 + `comments_has_more`), and media. |
| 5 | `get_page_blocks` | `page` | List the recursive block tree for a Notion page or block ID (`limit` default 50, `cursor`). Offline reads report a cache miss when no children have been fetched. Returns `has_more`, `next_cursor`, and `incomplete_blocks`. |
| 6 | `create_page` | `parent_id`, `title` | Create a new Notion page under a parent page or database from Markdown content. |
| 7 | `append_blocks` | `page`, `markdown` | Append Markdown or slash-style blocks (`/h1`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`) to a Notion page or block. |
| 8 | `update_block` | `block_id`, `text` | Edit one block's text using inline Markdown. Include the complete inline text so unchanged spans retain their formatting and mentions. Use `toggle_todo` for checkbox-only changes. |
| 9 | `toggle_todo` | `block_id` | Toggle or explicitly set a Notion `to_do` block's checkbox state. |
| 10 | `archive_page` | `page`, `confirm` | Archive (or unarchive) a Notion page. Destructive archive requires `confirm: true`. |
| 11 | `list_databases` | — | List accessible Notion databases with schemas (`limit` default 50, `cursor`). |
| 12 | `query_database` | `database` | Query rows of a Notion database with optional Notion API JSON `filter`, `sorts`, `limit` (default 50), and `cursor`. `filter` and `sorts` are rejected when `offline: true`. |
| 13 | `create_database_item` | `database`, `title` | Create a new row page inside a Notion database with title, optional status, custom properties, and Markdown body. |
| 14 | `update_database_item` | `page_id` | Update title, status, or arbitrary properties on a database row. `title` and `status` use the row's property names and types (`status` targets a `status` column and falls back to `select` only when no `status` column exists; pass an explicit column in `properties` to disambiguate multiple columns of the same type without conflicting values). |
| 15 | `list_comments` | `page` | List comments on a Notion page (`limit` default 50, `cursor`). With `offline: true`, comments that were never fetched or failed to fetch are a tool error. |
| 16 | `add_comment` | `page`, `text` | Add a comment to a Notion page or reply to an existing discussion thread. |
| 17 | `download_media` | `target` | Download a Notion-hosted or external media URL or image/file/pdf block ID to local disk. |
| 18 | `sync_now` | — | Synchronize Notion workspace pages and databases into the local SQLite WAL + FTS5 cache immediately. Streams every search batch into the cache; returns `fetched_count` and `complete`. |
| 19 | `update_page` | `page`, plus `title` or `icon` | Set a title or emoji icon. Fields you omit keep their current values. Send `null` to clear either field. |

Both compact and full `get_page` replies include `comments_error` when comment retrieval fails. Compact reads never call `/v1/comments`; they report the comment state already known to the local cache (`"cached"`, `"partial"`, `"failed"`, or `"not_fetched"`) without overwriting cached comments or prior errors. Full replies (`compact: false`) fetch comments (up to 2,000 + `comments_has_more`) and may include previously cached comments alongside `comments_error` on an offline read. A successful empty comment list has `comments: []`.

Tool text replies (`content[0].text`) carry minified JSON matching `structuredContent`.

### Compact `get_page` Fields

| Key | Type | Meaning |
|---|---|---|
| `id` | string | Page ID |
| `title` | string | Page title |
| `last_edited_time` | string \| null | Last edit timestamp |
| `markdown` | string | Full rendered Markdown body |
| `incomplete_blocks` | string[] | Block IDs whose children were not loaded (document order); empty when complete |
| `comments_state` | string | `"cached"`, `"partial"`, `"failed"`, or `"not_fetched"` |
| `comments_error` | string (optional) | Present only when `comments_state` is `"failed"` |
| `omitted.blocks` | integer | Top-level block count omitted from the compact view |
| `omitted.comments` | integer \| null | Cached comment count when `comments_state` is `"cached"`; `null` otherwise |
| `omitted.media` | integer | Omitted media reference count |
| `omitted.fields` | string[] | Omitted `page.*` fields |
| `hint` | string | Follow-up call to request the full page detail |

### Paginated List Envelope (`get_page_blocks`, `list_comments`, `list_databases`, `query_database`)

| Key | Type | Meaning |
|---|---|---|
| `ok` | boolean | Always `true` on success |
| `count` | integer | Number of items in this response |
| `returned` | integer | Same value as `count` |
| `blocks` / `comments` / `databases` / `rows` | array | Page of items |
| `has_more` | boolean | Whether additional items exist |
| `next_cursor` | string \| null | Cursor for the next call (`null` when no further cursor can be used) |
| `hint` | string (optional) | Present on offline partial comment cache exhaustion |

### Examples

Compact `get_page` (default):

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_page","arguments":{"page":"page-roadmap"}}}
```

Full `get_page`:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_page","arguments":{"page":"page-roadmap","compact":false}}}
```

Paginated `list_comments`:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_comments","arguments":{"page":"page-manycmt","limit":2,"cursor":"cur-2"}}}
```

Offline `query_database` rejecting `filter`:

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query_database","arguments":{"database":"db-tasks","offline":true,"filter":{"property":"Status","status":{"equals":"Done"}}}}}
```

For `update_page`, `{"page":"Q4 Launch Plan","title":"Q4 Launch Checklist"}` changes the title. `{"page":"Q4 Launch Checklist","icon":null}` removes its icon. A title update reads the page's title property first, including custom title names on database rows. The Notion connection needs read access for that lookup and update access for the patch. [Notion Update page API](https://developers.notion.com/reference/patch-page)

## Known limits

- **Block tree depth**: recursive block reads load up to 13 levels (depth cap 12 below the root). Any deeper blocks with unloaded children are reported in `incomplete_blocks` (JSON) and a stderr warning (text), and can be fetched directly with `notion blocks list <block-id>` / `get_page_blocks`.
- **`relation` and `people` properties**: Notion's page endpoint returns at most 25 entries per property; cached values reflect those entries.
- **`blocks list` / `get_page_blocks` `count`**: counts top-level blocks in the returned slice, not nested descendants.
- **Page detail comments**: full page detail JSON serializes at most 2,000 comments (a 2,001st entry is fetched only as lookahead and kept in the local cache) and reports `comments_has_more: true` when more exist. Use `notion comments list <page> --json --cursor ...` or `list_comments` with `cursor` to read the remainder.
- **Offline database queries**: `--offline` / `offline: true` cannot evaluate Notion `filter` or `sorts` against the local SQLite cache and returns an error instead of silently ignoring them.
