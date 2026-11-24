import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import { runPostinstall } from '../scripts/postinstall.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const POSTINSTALL_SRC = path.join(REPO_ROOT, 'scripts', 'postinstall.mjs');

function makeSimulatedInstall(rootDir, platform = process.platform, arch = process.arch) {
  const scopeDir = path.join(rootDir, 'node_modules', '@bitefoundry');
  const mainPkg = path.join(scopeDir, 'notion');
  const platPkg = path.join(scopeDir, `notion-${platform}-${arch}`);
  const dotBin = path.join(rootDir, 'node_modules', '.bin');

  fs.mkdirSync(path.join(mainPkg, 'bin'), { recursive: true });
  fs.mkdirSync(path.join(mainPkg, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(platPkg, 'bin'), { recursive: true });
  fs.mkdirSync(dotBin, { recursive: true });

  fs.writeFileSync(
    path.join(mainPkg, 'package.json'),
    JSON.stringify({ name: '@bitefoundry/notion', version: '0.0.0-test' }),
  );
  fs.writeFileSync(
    path.join(platPkg, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/notion-${platform}-${arch}`, version: '0.0.0-test' }),
  );

  fs.writeFileSync(path.join(mainPkg, 'bin', 'notion.js'), '// fallback js launcher\n');
  fs.copyFileSync(POSTINSTALL_SRC, path.join(mainPkg, 'scripts', 'postinstall.mjs'));

  const binaryName = platform === 'win32' ? 'notion.exe' : 'notion';
  const binaryBytes = 'FAKE-NATIVE-NOTION-BINARY';
  fs.writeFileSync(path.join(platPkg, 'bin', binaryName), binaryBytes);

  if (platform === 'win32') {
    fs.writeFileSync(
      path.join(dotBin, 'notion.cmd'),
      '@ECHO off\r\nnode "%~dp0\\..\\@bitefoundry\\notion\\bin\\notion.js" %*\r\n',
    );
  }

  return { mainPkg, platPkg, dotBin, binaryBytes };
}

test('runPostinstall skips inside development repository', () => {
  const result = runPostinstall({ pkgRoot: REPO_ROOT });
  assert.deepEqual(result, { skipped: true, reason: 'dev_repo' });
});

test('runPostinstall links native binary in simulated install', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'notion-postinstall-'));
  try {
    const { mainPkg, dotBin, binaryBytes } = makeSimulatedInstall(tmp);
    const result = runPostinstall({ pkgRoot: mainPkg });
    assert.equal(result.skipped, false);
    assert.equal(result.linked, 1);

    if (process.platform === 'win32') {
      assert.equal(
        fs.readFileSync(path.join(mainPkg, 'bin', 'notion.exe'), 'utf8'),
        binaryBytes,
      );
      const cmdContent = fs.readFileSync(path.join(dotBin, 'notion.cmd'), 'utf8');
      assert.ok(cmdContent.includes('notion.exe'), `expected rewritten cmd shim: ${cmdContent}`);
    } else {
      assert.equal(
        fs.readFileSync(path.join(mainPkg, 'bin', 'notion.js'), 'utf8'),
        binaryBytes,
      );
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('postinstall main-module check executes when invoked via symlinked path', (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'notion-postinstall-symlink-'));
  try {
    const { mainPkg, binaryBytes } = makeSimulatedInstall(tmp);
    const realScript = path.join(mainPkg, 'scripts', 'postinstall.mjs');
    let invokedScript = path.join(tmp, 'symlinked-postinstall.mjs');

    try {
      fs.symlinkSync(realScript, invokedScript);
    } catch (err) {
      if (process.platform === 'win32' && err && err.code === 'EPERM') {
        // Fall back to a Windows directory junction (does not require admin/Developer Mode)
        // so symlinked-path canonicalization is still exercised end-to-end on Windows.
        const junctionDir = path.join(tmp, 'linked-pkg');
        try {
          fs.symlinkSync(mainPkg, junctionDir, 'junction');
          invokedScript = path.join(junctionDir, 'scripts', 'postinstall.mjs');
        } catch (jErr) {
          if (jErr && jErr.code === 'EPERM') {
            t.skip('Windows symlink creation requires Developer Mode or Administrator privileges (EPERM)');
            return;
          }
          throw jErr;
        }
      } else {
        throw err;
      }
    }

    const res = spawnSync(process.execPath, [invokedScript], { encoding: 'utf8' });
    assert.equal(res.status, 0, `stderr: ${res.stderr}`);

    if (process.platform === 'win32') {
      assert.equal(
        fs.readFileSync(path.join(mainPkg, 'bin', 'notion.exe'), 'utf8'),
        binaryBytes,
      );
    } else {
      assert.equal(
        fs.readFileSync(path.join(mainPkg, 'bin', 'notion.js'), 'utf8'),
        binaryBytes,
      );
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
