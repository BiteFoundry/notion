# `@bitefoundry/notion` Local HTTP API Reference

Start the local HTTP JSON API server (default `127.0.0.1:8791`):

```bash
# No key given: a random key is generated for this run and printed once to stderr.
notion serve --port 8791
# Your own key, from the flag or from NOTION_SERVE_API_KEY (keeps it out of the process list):
notion serve --port 8791 --api-key "$NOTION_LOCAL_API_KEY"
NOTION_SERVE_API_KEY="$NOTION_LOCAL_API_KEY" notion serve
# A non-loopback bind always needs a key:
notion serve --host 0.0.0.0 --port 8791 --api-key "$NOTION_LOCAL_API_KEY"
```

## Authentication and browser access

Pass `x-api-key: <key>` or `Authorization: Bearer <key>` on all `/api/v1/*` routes. `GET /health` needs no key.

| Flag | Effect |
|---|---|
| `--api-key <key>` | Key to require. Falls back to the `NOTION_SERVE_API_KEY` environment variable. |
| _(no key)_ | On `127.0.0.1` / `::1` a 32-byte random key is generated and printed once to stderr as `API key (generated, valid for this run): <key>`. It is never written to disk. On any other bind address the server refuses to start. |
| `--allow-keyless` | Serve `/api/v1/*` without a key. Loopback binds only; prints a warning to stderr. |
| `--allowed-host <name>` | Accept one more `Host` header name (repeatable). |
| `--cors-origin <origin>` | Allow one exact browser origin such as `https://app.example.com` (repeatable). No wildcard. |

The server is closed to web pages by default:

- **No CORS headers** are sent unless `--cors-origin` is given, so a page on another origin cannot read a response or pass a preflight. With `--cors-origin`, only the listed origins get `Access-Control-Allow-Origin`, for `GET`, `POST`, `PATCH` and the headers `content-type`, `authorization`, `x-api-key`. The key is still required.
- **`Host` allowlist** (DNS-rebinding protection): on a loopback bind the `Host` header must name `127.0.0.1`, `localhost`, `[::1]`, the bind address, or an `--allowed-host` value; the port is ignored. Anything else returns `403 {"ok":false,"error":"Forbidden host"}`. On a non-loopback bind the check runs only when `--allowed-host` is given.
- **Cross-site requests**: a request with an `Origin` header that is not an allowed origin, or with `Sec-Fetch-Site: cross-site` / `same-site` and no allowed origin, returns `403 {"ok":false,"error":"Forbidden origin"}` before it reaches Notion.

Command-line clients (`curl`, scripts, agents) send none of those headers and are unaffected.

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
| `PATCH` | `/api/v1/pages/{id}` | Update page title, icon, properties, or archive/unarchive (`{ title?, icon?, archived?, properties? }`). Every field given is applied in one Notion call; a body with none of them returns HTTP 400. |
| `GET` | `/api/v1/pages/{id}/blocks?offline=false` | Get a page or block's recursive children. Offline reads return HTTP 404 for an uncached list and HTTP 200 for a verified empty list. |
| `POST` | `/api/v1/pages/{id}/blocks` | Append Markdown or slash-style blocks (`{ markdown }`) |
| `PATCH` | `/api/v1/blocks/{id}` | Update block text or toggle `to_do.checked` (`{ block_type?, text?, checked? }`) |
| `GET` | `/api/v1/databases?refresh=false&offline=false` | List databases & schemas |
| `POST` | `/api/v1/databases/{id}/query` | Query database rows (`{ filter?, sorts?, limit?, cursor?, offline? }` → `{ ok, count, rows, has_more, next_cursor }`). Pass `next_cursor` back as `cursor` for the next page. Returns HTTP 400 when `offline: true` is combined with `filter` or `sorts`. |
| `POST` | `/api/v1/databases/{id}/items` | Create a new database item (`{ title, status?, properties?, body? }`) |
| `GET` | `/api/v1/pages/{id}/comments?offline=false` | List page comments. Offline reads return HTTP 404 when comments have not been fetched or the last fetch failed. |
| `POST` | `/api/v1/pages/{id}/comments` | Add a comment or discussion reply (`{ text, discussion_id? }`) |
| `POST` | `/api/v1/sync?full=false` | Synchronize workspace pages & databases into SQLite WAL + FTS5 immediately (`{ ok, workspace_id, pages_count, databases_count, fetched_count, complete, full }`). `?full=true` rescans the whole workspace instead of stopping at the last sync point. |

See [`docs/openapi.yaml`](openapi.yaml) for full schemas.
