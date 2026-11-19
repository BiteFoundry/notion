# `@bitefoundry/notion` Local HTTP API Reference

Start the local HTTP JSON API server (default `127.0.0.1:8791`):

```bash
notion serve --port 8791
# Optional API key protection (required when binding non-loopback hosts):
notion serve --host 0.0.0.0 --port 8791 --api-key "$NOTION_LOCAL_API_KEY"
```

When `--api-key` is configured, pass `x-api-key: <key>` or `Authorization: Bearer <key>` on all `/api/v1/*` routes.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Service health & version (`{ ok, service: "notion-cli", version }`) |
| `GET` | `/api/v1/whoami` | Authenticated Notion bot/user and active workspace metadata |
| `GET` | `/api/v1/workspaces` | List configured Notion workspace profiles |
| `GET` | `/api/v1/search?q=<query>&remote=false&offline=false&limit=30` | Local SQLite FTS5 + Korean `<3` char LIKE search (or remote Notion search) |
| `GET` | `/api/v1/pages?archived=false&refresh=false&offline=false` | List pages in active workspace |
| `POST` | `/api/v1/pages` | Create a new page (`{ parent_id, title, icon?, markdown?, parent_is_database?, properties? }`) |
| `GET` | `/api/v1/pages/{id}?offline=false` | Get full page detail (metadata, properties, block tree, comments, markdown, media refs) |
| `PATCH` | `/api/v1/pages/{id}` | Update page title, icon, properties, or archive/unarchive (`{ title?, icon?, archived?, properties? }`) |
| `GET` | `/api/v1/pages/{id}/blocks?offline=false` | Get recursive block tree for a page |
| `POST` | `/api/v1/pages/{id}/blocks` | Append Markdown or slash-style blocks (`{ markdown }`) |
| `PATCH` | `/api/v1/blocks/{id}` | Update block text or toggle `to_do.checked` (`{ block_type?, text?, checked? }`) |
| `GET` | `/api/v1/databases?refresh=false&offline=false` | List databases & schemas |
| `POST` | `/api/v1/databases/{id}/query` | Query database rows (`{ filter?, sorts?, limit?, offline? }`) |
| `POST` | `/api/v1/databases/{id}/items` | Create a new database item (`{ title, status?, properties?, body? }`) |
| `GET` | `/api/v1/pages/{id}/comments?offline=false` | List page comments |
| `POST` | `/api/v1/pages/{id}/comments` | Add a comment or discussion reply (`{ text, discussion_id? }`) |
| `POST` | `/api/v1/sync` | Synchronize workspace pages & databases into SQLite WAL + FTS5 immediately |

See [`docs/openapi.yaml`](openapi.yaml) for full schemas.
