import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { assertBuildToolchain } from '../check-build-toolchain.mjs';

const toolsRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const dataMinerRoot = path.dirname(toolsRoot);
const guardPath = path.join(toolsRoot, 'check-build-toolchain.mjs');

function runGuard(nodeVersionText, pnpmVersionText) {
  return () => assertBuildToolchain({ nodeVersionText, pnpmVersionText });
}

function runCliWithPathPnpm(pnpmVersion, extraEnv = {}) {
  const fakeBin = mkdtempSync(path.join(tmpdir(), 'dataminer-fake-pnpm-'));
  try {
    const fakePnpm = path.join(fakeBin, process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm');
    writeFileSync(
      fakePnpm,
      process.platform === 'win32'
        ? `@echo off\r\necho ${pnpmVersion}\r\n`
        : `#!/bin/sh\necho ${pnpmVersion}\n`,
    );
    if (process.platform !== 'win32') chmodSync(fakePnpm, 0o755);

    return spawnSync(process.execPath, [guardPath], {
      encoding: 'utf8',
      env: {
        ...process.env,
        ...extraEnv,
        PATH: `${fakeBin}${path.delimiter}${process.env.PATH ?? ''}`,
      },
    });
  } finally {
    rmSync(fakeBin, { recursive: true, force: true });
  }
}

test('rejects Node versions outside Vite supported lines and lower bounds', () => {
  for (const nodeVersion of ['20.11.0', '20.18.99', '21.7.3', '22.11.99']) {
    assert.throws(runGuard(nodeVersion, '11.9.0'), (error) => {
      assert.match(error.message, new RegExp(`Node ${nodeVersion.replaceAll('.', '\\.')}`));
      assert.match(error.message, /\^20\.19\.0 \|\| >=22\.12\.0/);
      return true;
    });
  }
});

test('accepts Vite lower boundaries and newer supported Node releases', () => {
  for (const nodeVersion of ['20.19.0', '20.20.1', '22.12.0', '23.11.0', '24.19.0']) {
    assert.doesNotThrow(runGuard(nodeVersion, '11.9.0'), nodeVersion);
  }
});

test('rejects a package manager different from the root pin', () => {
  assert.throws(runGuard('22.12.0', '11.16.0'), (error) => {
    assert.match(error.message, /pnpm 11\.16\.0/);
    assert.match(error.message, /pnpm@11\.9\.0/);
    return true;
  });
});

test('CLI ignores the former test override environment variables', () => {
  const result = runCliWithPathPnpm('11.9.0', {
    DATAMINER_TEST_NODE_VERSION: '20.11.0',
    DATAMINER_TEST_PNPM_VERSION: '11.16.0',
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Node \d+\.\d+\.\d+, pnpm@11\.9\.0/);
});

test('direct CLI validates the pnpm executable that installer scripts will invoke', () => {
  const result = runCliWithPathPnpm('11.16.0', {
    // A correct inherited user-agent must not hide the executable that the
    // installer will actually invoke after this guard returns.
    npm_config_user_agent: 'pnpm/11.9.0 npm/? node/v24.19.0 win32 x64',
  });
  assert.notEqual(result.status, 0, 'guard unexpectedly accepted the wrong PATH pnpm');
  assert.match(result.stderr, /Unsupported pnpm 11\.16\.0/);
});

test('root build and bundle entry points retain the fail-fast toolchain wiring', () => {
  const packageJson = JSON.parse(readFileSync(path.join(dataMinerRoot, 'package.json'), 'utf8'));
  for (const scriptName of [
    'build',
    'bundle:windows:dev',
    'bundle:windows:all-games-dev',
  ]) {
    assert.equal(
      packageJson.scripts[`pre${scriptName}`],
      'node ./tools/check-build-toolchain.mjs',
      `missing fail-fast hook for ${scriptName}`,
    );
  }

  for (const installerName of [
    'build-windows-installer.ps1',
  ]) {
    const installer = readFileSync(path.join(toolsRoot, installerName), 'utf8');
    const guardOffset = installer.indexOf('tools\\check-build-toolchain.mjs');
    const firstCargoOffset = installer.indexOf("Invoke-NativeChecked -Command 'cargo'");
    const firstTauriOffset = installer.indexOf("'tauri', 'build'");
    assert.notEqual(guardOffset, -1, `${installerName} does not call the shared guard`);
    assert.ok(firstCargoOffset === -1 || guardOffset < firstCargoOffset, `${installerName} guards after Cargo`);
    assert.ok(firstTauriOffset === -1 || guardOffset < firstTauriOffset, `${installerName} guards after Tauri`);
  }
});
