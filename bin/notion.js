#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const PLATFORM_PACKAGES = {
  'win32-x64': '@shinick/notion-win32-x64',
  'win32-arm64': '@shinick/notion-win32-arm64',
  'darwin-x64': '@shinick/notion-darwin-x64',
  'darwin-arm64': '@shinick/notion-darwin-arm64',
  'linux-x64': '@shinick/notion-linux-x64',
  'linux-arm64': '@shinick/notion-linux-arm64',
};

function resolveBinary() {
  if (process.env.NOTION_BIN_PATH && fs.existsSync(process.env.NOTION_BIN_PATH)) {
    return process.env.NOTION_BIN_PATH;
  }

  const binaryName = process.platform === 'win32' ? 'notion.exe' : 'notion';
  const localLinked = path.join(__dirname, binaryName);
  if (fs.existsSync(localLinked)) {
    return localLinked;
  }

  const key = `${process.platform}-${process.arch}`;
  const pkgName = PLATFORM_PACKAGES[key];
  if (!pkgName) {
    console.error(
      `[@shinick/notion] Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORM_PACKAGES).join(', ')}`,
    );
    process.exit(1);
  }

  try {
    const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
    const candidate = path.join(path.dirname(pkgJsonPath), 'bin', binaryName);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  } catch {
    // Fall through to helpful diagnostic error below.
  }

  console.error(
    `[@shinick/notion] Could not locate prebuilt binary from optional dependency "${pkgName}".\n` +
      `Try reinstalling without --omit=optional, or build from source with \`cargo build --release\`.`,
  );
  process.exit(1);
}

const binPath = resolveBinary();
const result = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`[@shinick/notion] Failed to launch ${binPath}: ${result.error.message}`);
  process.exit(1);
}

if (result.signal) {
  process.kill(process.pid, result.signal);
}

process.exit(result.status ?? 0);
