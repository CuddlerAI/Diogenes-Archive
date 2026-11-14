import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const dataMinerRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const packageJson = JSON.parse(readFileSync(path.join(dataMinerRoot, 'package.json'), 'utf8'));
const requiredNodeRange = packageJson.engines?.node;
const requiredPackageManager = packageJson.packageManager;
const requiredPnpmEngine = packageJson.engines?.pnpm;

export function parseVersion(value, label) {
  const match = /^(?:v)?(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/.exec(value.trim());
  if (!match) throw new Error(`Could not parse ${label} version: ${value}`);
  return match.slice(1).map(Number);
}

function compareVersion(left, right) {
  for (let index = 0; index < 3; index += 1) {
    if (left[index] !== right[index]) return left[index] - right[index];
  }
  return 0;
}

export function supportsViteNode(version) {
  return (
    (version[0] === 20 && compareVersion(version, [20, 19, 0]) >= 0) ||
    compareVersion(version, [22, 12, 0]) >= 0
  );
}

function readPnpmVersion() {
  const candidates = process.platform === 'win32'
    ? [
        [process.env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', 'pnpm --version']],
        [process.env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', 'corepack pnpm --version']],
      ]
    : [
        ['pnpm', ['--version']],
        ['corepack', ['pnpm', '--version']],
      ];
  for (const [command, args] of candidates) {
    try {
      return execFileSync(command, args, { encoding: 'utf8' }).trim();
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
  }
  throw new Error('Could not execute pnpm --version or corepack pnpm --version.');
}

export function assertNodeVersion(nodeVersionText) {
  const nodeVersion = parseVersion(nodeVersionText, 'Node');
  if (requiredNodeRange !== '^20.19.0 || >=22.12.0') {
    throw new Error(`package.json engines.node drifted from the Vite contract: ${requiredNodeRange}`);
  }
  if (!supportsViteNode(nodeVersion)) {
    throw new Error(
      `Unsupported Node ${nodeVersionText}; DataMiner requires ${requiredNodeRange}. ` +
        'Install the recommended Node 22 release before building.',
    );
  }
}

export function assertPnpmVersion(pnpmVersionText) {
  const expectedPnpmVersion = /^pnpm@(\d+\.\d+\.\d+)$/.exec(requiredPackageManager)?.[1];
  if (!expectedPnpmVersion) {
    throw new Error(`package.json packageManager must be an exact pnpm pin, got: ${requiredPackageManager}`);
  }
  if (requiredPnpmEngine !== undefined && requiredPnpmEngine !== expectedPnpmVersion) {
    throw new Error(
      `package.json engines.pnpm must be absent or match ${requiredPackageManager}, got: ${requiredPnpmEngine}`,
    );
  }
  parseVersion(pnpmVersionText, 'pnpm');
  if (pnpmVersionText !== expectedPnpmVersion) {
    throw new Error(
      `Unsupported pnpm ${pnpmVersionText}; DataMiner pins ${requiredPackageManager}. ` +
      `Run this workspace through Corepack or install pnpm ${expectedPnpmVersion}.`,
    );
  }
}

export function assertBuildToolchain({ nodeVersionText, pnpmVersionText }) {
  assertNodeVersion(nodeVersionText);
  assertPnpmVersion(pnpmVersionText);
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  const nodeVersionText = process.versions.node;
  assertNodeVersion(nodeVersionText);
  const pnpmVersionText = readPnpmVersion();
  assertPnpmVersion(pnpmVersionText);
  console.log(`Build toolchain OK: Node ${nodeVersionText}, ${requiredPackageManager}`);
}
