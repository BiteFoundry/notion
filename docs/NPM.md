# `@shinick/notion` 6-Platform npm Distribution

`@shinick/notion` distributes prebuilt native Rust binaries via npm `optionalDependencies`, installing both `notion` and `nt` commands with zero Node.js runtime overhead after `postinstall` hardlinking.

## Platform Packages

| Platform Package | Rust Target | Binary |
|---|---|---|
| `@shinick/notion-win32-x64` | `x86_64-pc-windows-msvc` | `bin/notion.exe` |
| `@shinick/notion-win32-arm64` | `aarch64-pc-windows-msvc` | `bin/notion.exe` |
| `@shinick/notion-darwin-x64` | `x86_64-apple-darwin` | `bin/notion` |
| `@shinick/notion-darwin-arm64` | `aarch64-apple-darwin` | `bin/notion` |
| `@shinick/notion-linux-x64` | `x86_64-unknown-linux-gnu.2.28` | `bin/notion` |
| `@shinick/notion-linux-arm64` | `aarch64-unknown-linux-gnu.2.28` | `bin/notion` |

## Packaging Scripts

- `node scripts/sync-version.mjs --check`: verifies `Cargo.toml`, root `package.json`, and all 6 `npm/notion-*/package.json` manifests match version `0.1.0`.
- `node scripts/verify-glibc.mjs --self-test`: verifies the GLIBC <= 2.28 ELF symbol checker used in GitHub Actions Linux releases.
- `node scripts/build-npm.mjs [--target <triple>] [--all]`: stages Cargo release binaries into `npm/notion-*/bin/`.
- `scripts/postinstall.mjs`: links `notion` and `nt` directly to the native binary on Unix/Windows while guarding against mutating a git development checkout.
