# `@bitefoundry/notion` Local HTTP API Reference

Start the local HTTP JSON API server (default `127.0.0.1:8791`):

```bash
notion serve --port 8791
# Optional API key protection (required when binding non-loopback hosts):
notion serve --host 0.0.0.0 --port 8791 --api-key "$NOTION_LOCAL_API_KEY"
```

When `--api-key` is configured, pass `x-api-key: <key>` or `Authorization: Bearer <key>` on all `/api/v1/*` routes.

Errors return `{ "ok": false, "error": "..." }` and preserve the upstream Notion API HTTP status code (such as `401`, `403`, `404`, `409`, or `429`) when a request fails upstream.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Service health & version (`{ ok, service: "notion-cli", version }`) |
| `GET` | `/api/v1/whoami` | Authenticated Notion bot/user and active workspace metadata |
| `GET` | `/api/v1/workspaces` | List configured Notion workspace profiles |
| `GET` | `/api/v1/search?q=<query>&remote=false&offline=false&limit=30` | Local SQLite FTS5 + Korean `<3` char LIKE search (or remote Notion search) |
| `GET` | `/api/v1/pages?archived=false&refresh=false&offline=false` | List pages in active workspace |
| `POST` | `/api/v1/pages` | Create a new page (`{ parent_id, title, icon?, markdown?, parent_is_database?, properties? }`) |
| `GET` | `/api/v1/pages/{id}?offline=false` | Get full page detail (`detail` includes metadata, properties, block tree, comments, markdown, media refs, `incomplete_blocks`, and `comments_has_more`). Offline reads return HTTP 404 when the page body has not been fetched. |
| `PATCH` | `/api/v1/pages/{id}` | Update page title, icon, properties, or archive/unarchive (`{ title?, icon?, archived?, properties? }`) |
| `GET` | `/api/v1/pages/{id}/blocks?offline=false` | Get a page or block's recursive children. Offline reads return HTTP 404 for an uncached list and HTTP 200 for a verified empty list. |
| `POST` | `/api/v1/pages/{id}/blocks` | Append Markdown or slash-style blocks (`{ markdown }`) |
| `PATCH` | `/api/v1/blocks/{id}` | Update block text or toggle `to_do.checked` (`{ block_type?, text?, checked? }`) |
| `GET` | `/api/v1/databases?refresh=false&offline=false` | List databases & schemas |
| `POST` | `/api/v1/databases/{id}/query` | Query database rows (`{ filter?, sorts?, limit?, offline? }`). Returns HTTP 400 when `offline: true` is combined with `filter` or `sorts`. |
| `POST` | `/api/v1/databases/{id}/items` | Create a new database item (`{ title, status?, properties?, body? }`) |
| `GET` | `/api/v1/pages/{id}/comments?offline=false` | List page comments. Offline reads return HTTP 404 when comments have not been fetched or the last fetch failed. |
| `POST` | `/api/v1/pages/{id}/comments` | Add a comment or discussion reply (`{ text, discussion_id? }`) |
| `POST` | `/api/v1/sync` | Synchronize workspace pages & databases into SQLite WAL + FTS5 immediately (`{ ok, workspace_id, pages_count, databases_count, fetched_count, complete }`) |

See [`docs/openapi.yaml`](openapi.yaml) for full schemas.
