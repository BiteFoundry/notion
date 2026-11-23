# `@bitefoundry/notion` MCP Server Reference (20 Tools)

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

On a network that inspects HTTPS, pass the proxy's root CA through the server's `env`; MCP hosts do not always inherit your shell environment:

```json
{
  "mcpServers": {
    "notion": {
      "command": "notion",
      "args": ["mcp"],
      "env": { "NOTION_EXTRA_CA_PEM": "/etc/ssl/corp-root-ca.pem" }
    }
  }
}
```

`SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS` are honoured too. Warnings about them go to stderr, never to the JSON-RPC stream on stdout.

## Available Tools (20)

| # | Tool | Required Args | Description |
|---|---|---|---|
| 1 | `whoami` | — | Return the currently authenticated Notion user/bot and active workspace metadata. |
| 2 | `list_workspaces` | — | List all configured Notion workspace profiles and the currently active workspace. |
| 3 | `list_users` | — | List Notion workspace users (`limit` default 50, `cursor`) with `id`, `name`, `user_type`, and `email`. |
| 4 | `search_pages` | `query` | Search Notion pages and databases using local SQLite FTS5 (with Korean `<3` char LIKE fallback) or remote Notion Search API. |
| 5 | `get_page` | `page` | Fetch a Notion page. Compact by default (`compact: true`): returns `id`, `title`, `last_edited_time`, `markdown`, `incomplete_blocks`, `comments_state`, `omitted` counts, and a `hint` without requesting `/v1/comments`. Pass `compact: false` for properties, block tree, comments (up to 2,000 + `comments_has_more`), and media. |
| 6 | `get_page_blocks` | `page` | List the recursive block tree for a Notion page or block ID (`limit` default 50, `cursor`). Offline reads report a cache miss when no children have been fetched. Returns `has_more`, `next_cursor`, and `incomplete_blocks`. |
| 7 | `create_page` | `parent_id`, `title` | Create a new Notion page under a parent page or database from Markdown content. Automatically chunks bodies over 100 blocks. |
| 8 | `append_blocks` | `page`, `markdown` | Append Markdown or slash-style blocks (`/h1`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`) to a Notion page or block. Compact by default (`compact: true`): returns `{ ok, appended, block_ids }`. Pass `compact: false` for full block objects. |
| 9 | `update_block` | `block_id`, `text` | Edit one block's text using inline Markdown. Include the complete inline text so unchanged spans retain their formatting and mentions. Use `toggle_todo` for checkbox-only changes. |
| 10 | `toggle_todo` | `block_id` | Toggle or explicitly set a Notion `to_do` block's checkbox state. |
| 11 | `archive_page` | `page`, `confirm` | Archive (or unarchive) a Notion page. Destructive archive requires `confirm: true`. |
| 12 | `list_databases` | — | List accessible Notion databases with schemas (`limit` default 50, `cursor`). |
| 13 | `query_database` | `database` | Query rows of a Notion database with optional Notion API JSON `filter`, `sorts`, `limit` (default 50), and `cursor`. `filter` and `sorts` are rejected when `offline: true`. |
| 14 | `create_database_item` | `database`, `title` | Create a new row page inside a Notion database with title, optional status, custom properties, and Markdown body. |
| 15 | `update_database_item` | `page_id` | Update title, status, or arbitrary properties on a database row. `page_id` accepts a row UUID, Notion URL, or exact row title. `title` and `status` use the row's property names and types (`status` targets a `status` column and falls back to `select` only when no `status` column exists; pass an explicit column in `properties` to disambiguate multiple columns of the same type without conflicting values). |
| 16 | `list_comments` | `page` | List comments on a Notion page (`limit` default 50, `cursor`). With `offline: true`, comments that were never fetched or failed to fetch are a tool error. |
| 17 | `add_comment` | `page`, `text` | Add a comment to a Notion page or reply to an existing discussion thread. |
| 18 | `download_media` | `target` | Download a Notion-hosted or external media URL or image/file/pdf block ID to local disk. |
| 19 | `sync_now` | — | Synchronize Notion workspace pages and databases into the local SQLite WAL + FTS5 cache immediately. Streams every search batch into the cache; returns `fetched_count` and `complete`. |
| 20 | `update_page` | `page`, plus `title` or `icon` | Set a title or emoji icon. Fields you omit keep their current values. Send `null` to clear either field. |

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

### Paginated List Envelope (`get_page_blocks`, `list_comments`, `list_databases`, `query_database`, `list_users`)

| Key | Type | Meaning |
|---|---|---|
| `ok` | boolean | Always `true` on success |
| `count` | integer | Number of items in this response |
| `returned` | integer | Same value as `count` |
| `blocks` / `comments` / `databases` / `rows` / `users` | array | Page of items |
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

Write tools (`create_page`, `update_page`, `append_blocks`, `archive_page`, `create_database_item`, `update_database_item`, `add_comment`) require a single exact title match among active pages/databases when given a title instead of a UUID or Notion URL. When multiple pages share a title, the tool fails without writing and lists the candidate IDs:

```text
Error: Title 'Weekly Sync' matches 2 pages. Pass a page ID or URL: 22222222-2222-2222-2222-222222222221 (edited 2026-11-22T12:00:00.000Z), 22222222-2222-2222-2222-222222222222 (edited 2026-11-22T12:05:00.000Z)
```

## Known limits

- **Block tree depth**: recursive block reads load up to 13 levels (depth cap 12 below the root). Any deeper blocks with unloaded children are reported in `incomplete_blocks` (JSON) and a stderr warning (text), and can be fetched directly with `notion blocks list <block-id>` / `get_page_blocks`.
- **`relation` and `people` properties**: Notion's page endpoint returns at most 25 entries per property; cached values reflect those entries.
- **`blocks list` / `get_page_blocks` `count`**: counts top-level blocks in the returned slice, not nested descendants.
- **Page detail comments**: full page detail JSON serializes at most 2,000 comments (a 2,001st entry is fetched only as lookahead and kept in the local cache) and reports `comments_has_more: true` when more exist. Use `notion comments list <page> --json --cursor ...` or `list_comments` with `cursor` to read the remainder.
- **Offline database queries**: `--offline` / `offline: true` cannot evaluate Notion `filter` or `sorts` against the local SQLite cache and returns an error instead of silently ignoring them.
