# `@shinick/notion` (`notion` / `nt`)

Notion-native Terminal UI, CLI, local HTTP API (`:8791`), and stdio MCP server (18 tools) built in Rust.

## Highlights

- **Notion-Native Terminal UI (`notion` / `nt`)**:
  - Minimal 1-line top Breadcrumb Bar (`Workspace ▾ / Parent / 📄 Page`) with `[Width: Centered|Full]`, `[Sync]`, `[? Help]`
  - Collapsible Workspace Sidebar (`[` / `Ctrl+B`): **Favorites** (`★`), hierarchical collapsible **Workspace Tree** (`▸`/`▾`, `📄`, `🗂`), **Databases**, and **Recent / Trash**
  - **Editorial Page Canvas**: centered ~88-column measure by default (`w` toggles Full Width), properties strip with Notion's 10 canonical pill colors, live interactive `to_do` checkboxes (`Space`/`x` -> `PATCH /v1/blocks/{id}`), live expandable `toggle` blocks (`Enter`/`Space`), left-accented tinted Callouts, bordered Code Blocks (`y` copies code), aligned Unicode Tables, inline terminal graphics (Kitty / iTerm2 / Sixel / Halfblock) + `v`/`Alt+V` HD preview, and right-split **Comments Peek Drawer** (`C`)
  - **Database Views**: switch live between **`1 Table`**, **`2 Board` (Kanban)** with `h`/`l` columns & `m` card moves, and **`3 List`**, plus `f` Filter, `s` Sort, and `n` New Item
  - **Global Overlays**: `Ctrl+K`/`Ctrl+P` Quick Find & Command Palette, `Ctrl+S` Multi-Workspace Switcher, `Ctrl+,` Settings (5 curated palettes: Clean Dark, Warm Paper, Tokyo Night, Catppuccin, Nord), `Ctrl+F` File Picker with `z` zoxide jump, `u` Link Extractor, `.` Context Menu, `?` Help
- **Dual Authentication & Multi-Workspace**:
  - Internal Integration Token (`notion login --token ntn_...` or `NOTION_TOKEN`), validated via `GET /v1/users/me`
  - Public OAuth 2.0 loopback (`notion login --oauth --client-id <id> --client-secret <secret>`)
  - OS Keyring storage (`notion-cli`) with `0600` file fallback + `notion workspaces list|use|add|remove`
- **Local SQLite WAL + FTS5 Cache**:
  - Workspace-scoped `pages`, `databases`, `blocks`, `comments`, and `pages_fts` with automatic **Korean `<3` char `LIKE` fallback**
  - Optional single-instance background sync IPC daemon (`notion daemon start|status|stop|restart`)
- **AI Agent Ready**:
  - Local HTTP JSON API (`notion serve`, default `127.0.0.1:8791`)
  - Stdio JSON-RPC 2.0 MCP server (`notion mcp`, 18 tools)

## Install

```bash
npm install -g @shinick/notion
```

Both `notion` and `nt` commands are installed and linked directly to the native prebuilt Rust binary on Windows (`x64`/`arm64`), macOS (`arm64`/`x64`), and Linux (`x64`/`arm64`, GLIBC <= 2.28).

## Quick Start

```bash
# 1. Authenticate with an Internal Integration Token (30-second setup)
notion login --token ntn_...

# Or authenticate with Public OAuth 2.0
notion login --oauth --client-id <id> --client-secret <secret>

# 2. Sync your workspace into SQLite WAL + FTS5
notion sync

# 3. Launch the Notion-Native Terminal UI
notion      # or: nt

# 4. Use the CLI
notion search "Roadmap" --json
notion pages list --json
notion pages get "Product Roadmap"
notion pages append "Product Roadmap" "/todo Ship v0.1.0"
notion pages export "Product Roadmap" --out roadmap.md
notion blocks toggle <block-id>
notion db list
notion db query "Sprint Tasks" --json
notion db create-item "Sprint Tasks" --title "Write docs" --status "In Progress"
notion comments add "Product Roadmap" "Ready for review!"
notion download <url-or-block-id> --json

# 5. Manage multiple workspaces
notion workspaces list
notion workspaces add work --token ntn_...
notion workspaces use work
notion --workspace work pages list

# 6. Run HTTP API (:8791) or MCP Server
notion serve --port 8791
notion mcp
```

## Documentation

- [HTTP API Reference](docs/API.md) & [OpenAPI Spec](docs/openapi.yaml)
- [MCP Server Tools (18 Tools)](docs/MCP.md)
- [npm Cross-Platform Packaging](docs/NPM.md)
- [Release Notes v0.1.0](release-notes/v0.1.0.md)
