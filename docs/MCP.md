# `@bitefoundry/notion` MCP Server Reference (18 Tools)

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

## Available Tools (18)

| # | Tool | Required Args | Description |
|---|---|---|---|
| 1 | `whoami` | — | Return the currently authenticated Notion user/bot and active workspace metadata. |
| 2 | `list_workspaces` | — | List all configured Notion workspace profiles and the currently active workspace. |
| 3 | `search_pages` | `query` | Search Notion pages and databases using local SQLite FTS5 (with Korean `<3` char LIKE fallback) or remote Notion Search API. |
| 4 | `get_page` | `page` | Fetch a Notion page with properties, recursive block tree, Markdown export, comments, and media references. |
| 5 | `get_page_blocks` | `page` | List the recursive block tree for a Notion page or block ID. |
| 6 | `create_page` | `parent_id`, `title` | Create a new Notion page under a parent page or database from Markdown content. |
| 7 | `append_blocks` | `page`, `markdown` | Append Markdown or slash-style blocks (`/h1`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`) to a Notion page or block. |
| 8 | `update_block` | `block_id`, `text` | Update an existing Notion block's rich text or `to_do` checked status. |
| 9 | `toggle_todo` | `block_id` | Toggle or explicitly set a Notion `to_do` block's checkbox state. |
| 10 | `archive_page` | `page`, `confirm` | Archive (or unarchive) a Notion page. Destructive archive requires `confirm: true`. |
| 11 | `list_databases` | — | List accessible Notion databases with schemas. |
| 12 | `query_database` | `database` | Query rows of a Notion database with optional Notion API JSON filter and sorts. |
| 13 | `create_database_item` | `database`, `title` | Create a new row page inside a Notion database with title, optional status, custom properties, and Markdown body. |
| 14 | `update_database_item` | `page_id` | Update title, status, or arbitrary Notion properties on a database row page. |
| 15 | `list_comments` | `page` | List comments on a Notion page. |
| 16 | `add_comment` | `page`, `text` | Add a comment to a Notion page or reply to an existing discussion thread. |
| 17 | `download_media` | `target` | Download a Notion-hosted or external media URL or image/file/pdf block ID to local disk. |
| 18 | `sync_now` | — | Synchronize Notion workspace pages and databases into the local SQLite WAL + FTS5 cache immediately. |
