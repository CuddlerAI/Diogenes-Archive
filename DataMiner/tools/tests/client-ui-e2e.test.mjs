import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const dataMinerRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const ciWorkflowPath = path.join(dataMinerRoot, "..", ".github", "workflows", "dataminer-ci.yml");
const driverPath = path.join(dataMinerRoot, "tools", "client-ui-e2e", "cdp-drive.mjs");
const runnerPath = path.join(dataMinerRoot, "tools", "client-ui-e2e", "run-e2e.ps1");
const mockPath = path.join(dataMinerRoot, "tools", "client-ui-e2e", "mock-app-api.mjs");
const maliciousUrl = "http://must-not-be-exported.invalid/?credential=must-not-be-exported";
const maliciousMarker = "must-not-be-exported";
const safeReason = "account base URL must be https";

function enforceRequiredWindowsRunnerPlatform(platform, requirement) {
  if (requirement === "1" && platform !== "win32") {
    throw new Error(`Windows client UI E2E runner contract requires win32, received ${platform}`);
  }
}

enforceRequiredWindowsRunnerPlatform(
  process.platform,
  process.env.DATAMINER_REQUIRE_WINDOWS_RUNNER_TESTS,
);

function runnerTestForPlatform(platform, register = test) {
  return platform === "win32"
    ? register
    : (name, optionsOrFn, maybeFn) => register.skip(name, optionsOrFn, maybeFn);
}

const windowsTest = runnerTestForPlatform(process.platform);

test("PowerShell runner cases are explicit skips off Windows and execute on Windows", () => {
  const registered = [];
  const register = (name) => registered.push(name);
  register.skip = (name) => registered.push(`skip:${name}`);

  runnerTestForPlatform("linux", register)("runner-linux");
  runnerTestForPlatform("darwin", register)("runner-macos");
  assert.deepEqual(registered, ["skip:runner-linux", "skip:runner-macos"]);

  runnerTestForPlatform("win32", register)("runner-windows");
  assert.deepEqual(registered, ["skip:runner-linux", "skip:runner-macos", "runner-windows"]);
});

test("a required Windows runner gate fails closed on the wrong CI platform", () => {
  assert.doesNotThrow(() => enforceRequiredWindowsRunnerPlatform("win32", "1"));
  assert.doesNotThrow(() => enforceRequiredWindowsRunnerPlatform("linux", undefined));
  assert.throws(
    () => enforceRequiredWindowsRunnerPlatform("linux", "1"),
    /requires win32/i,
  );
});

test("Windows CI runs every PowerShell contract from its canonical dedicated temp root", () => {
  const source = readFileSync(ciWorkflowPath, "utf8");
  const releaseSurface = source.match(/^  release-surface:\r?\n[\s\S]*?(?=^  [a-z][a-z0-9-]*:\r?\n|\z)/m)?.[0];

  assert.ok(releaseSurface, "release-surface job must remain present");
  assert.match(releaseSurface, /^    runs-on: windows-latest$/m);
  assert.match(releaseSurface, /^          DATAMINER_REQUIRE_WINDOWS_RUNNER_TESTS: '1'$/m);
  assert.match(releaseSurface, /^          TEMP: \$\{\{ runner\.temp \}\}$/m);
  assert.match(releaseSurface, /^          TMP: \$\{\{ runner\.temp \}\}$/m);
  assert.match(releaseSurface, /^        run: node --test \.\/tools\/tests\/client-ui-e2e\.test\.mjs$/m);
  assert.doesNotMatch(releaseSurface, /continue-on-error/);
});

function diagnosticExport(reason = safeReason) {
  return {
    schema_version: "dm-diagnostic-export-1",
    created_unix_ms: 2_000_000_000_001,
    events: [
      {
        schema_version: "dm-diagnostic-event-1",
        timestamp_unix_ms: 2_000_000_000_000,
        kind: "service_startup_degraded",
        data: {
          service: "account",
          code: "account_base_url_invalid",
          reason,
        },
      },
    ],
  };
}

function invalidAccountStatus() {
  return {
    schemaVersion: "dm-account-status-2",
    availability: {
      kind: "invalid",
      code: "account_base_url_invalid",
      message: "账号服务地址必须使用 https。",
    },
    identity: { kind: "signedOut" },
  };
}

function writeDriverFixture(overrides = {}) {
  const root = mkdtempSync(path.join(tmpdir(), "dm-client-e2e-evidence-"));
  writeFileSync(
    path.join(root, "account-status.json"),
    JSON.stringify(overrides.status ?? invalidAccountStatus()),
  );
  writeFileSync(
    path.join(root, "account-view.txt"),
    overrides.viewText ?? "账号服务配置无效\n账号服务地址必须使用 https。",
  );
  writeFileSync(
    path.join(root, "diagnostic-export.json"),
    JSON.stringify(overrides.diagnostic ?? diagnosticExport()),
  );
  writeFileSync(
    path.join(root, "report.json"),
    JSON.stringify(overrides.report ?? { console: [], steps: [], verdict: { ok: true } }),
  );
  return root;
}

function verifyFixture(root) {
  return spawnSync(
    process.execPath,
    [
      driverPath,
      "--verify-evidence",
      root,
      "--forbidden-account-url",
      maliciousUrl,
      "--started-unix-ms",
      "1999999999999",
      "--out",
      root,
    ],
    { encoding: "utf8", timeout: 3_000 },
  );
}

test("driver accepts only the invalid-account UI, v2 IPC status, and safe startup diagnostic evidence", () => {
  const result = verifyFixture(writeDriverFixture());

  assert.equal(result.status, 0, result.stderr || result.stdout || String(result.error));
  assert.match(result.stdout, /E2E_EVIDENCE_VERIFIED/);
});

test("driver rejects an unconfigured UI/status masquerading as F2 evidence", () => {
  const root = writeDriverFixture({
    status: {
      schemaVersion: "dm-account-status-2",
      availability: { kind: "unconfigured" },
      identity: { kind: "signedOut" },
    },
    viewText: "账号服务未配置",
  });

  const result = verifyFixture(root);

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /configured invalid|配置无效/i);
});

test("driver rejects an account status from the obsolete IPC schema", () => {
  const status = invalidAccountStatus();
  status.schemaVersion = "dm-account-status-1";

  const result = verifyFixture(writeDriverFixture({ status }));

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /dm-account-status-2/);
});

test("driver rejects an invalid account status without account_base_url_invalid", () => {
  const status = invalidAccountStatus();
  status.availability.code = "account_configuration_invalid";

  const result = verifyFixture(writeDriverFixture({ status }));

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /account_base_url_invalid/);
});

test("driver rejects a diagnostic export without the account startup-degraded event", () => {
  const result = verifyFixture(writeDriverFixture({
    diagnostic: {
      schema_version: "dm-diagnostic-export-1",
      created_unix_ms: 2_000_000_000_001,
      events: [],
    },
  }));

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /service_startup_degraded/);
});

test("driver rejects an account startup diagnostic retained from an earlier run", () => {
  const diagnostic = diagnosticExport();
  diagnostic.events[0].timestamp_unix_ms = 1_999_999_999_998;

  const result = verifyFixture(writeDriverFixture({ diagnostic }));

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /current export snapshot/i);
});

test("driver rejects a diagnostic export that leaks the configured malicious URL", () => {
  const diagnostic = diagnosticExport();
  diagnostic.events.push({ kind: "unexpected", data: { rawUrl: maliciousUrl } });
  const root = writeDriverFixture({ diagnostic });

  const result = verifyFixture(root);

  assert.notEqual(result.status, 0);
  assert.doesNotMatch(`${result.stdout}\n${result.stderr}`, new RegExp(maliciousMarker));
  assert.doesNotMatch(`${result.stdout}\n${result.stderr}`, new RegExp(maliciousUrl.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

for (const [surface, overrides] of [
  ["account status", { status: { ...invalidAccountStatus(), availability: { ...invalidAccountStatus().availability, message: maliciousUrl } } }],
  ["account view", { viewText: `账号服务配置无效\n${maliciousUrl}` }],
  ["report console", { report: { console: [{ level: "error", text: maliciousUrl }], steps: [], verdict: { ok: true } } }],
  ["report marker", { report: { console: [], steps: [{ error: maliciousMarker }], verdict: { ok: false } } }],
]) {
  test(`driver rejects ${surface} leakage without echoing the secret in its failure output`, () => {
    const result = verifyFixture(writeDriverFixture(overrides));
    const output = `${result.stdout}\n${result.stderr}`;

    assert.notEqual(result.status, 0);
    assert.match(output, /E2E_EVIDENCE_INVALID/);
    assert.doesNotMatch(output, new RegExp(maliciousMarker));
    assert.doesNotMatch(output, new RegExp(maliciousUrl.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  });
}

test("driver rejects an account startup diagnostic dated after the export snapshot", () => {
  const diagnostic = diagnosticExport();
  diagnostic.events[0].timestamp_unix_ms = diagnostic.created_unix_ms + 1;

  const result = verifyFixture(writeDriverFixture({ diagnostic }));

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /export snapshot|future/i);
});

test("invalid-configuration driver has no account mutation path after its safety assertions", () => {
  const source = readFileSync(driverPath, "utf8");

  assert.doesNotMatch(source, /step\("account-fill-signup"/);
  assert.doesNotMatch(source, /step\("account-submit-signup"/);
  assert.doesNotMatch(source, /step\("account-verify-(?:wrong|right)-code"/);
  assert.doesNotMatch(source, /DM-E2E-TEST-0001|e2e@example\.com|E2e-passw0rd/);
});

test("required driver safety assertions abort instead of returning a failed step", () => {
  const source = readFileSync(driverPath, "utf8");

  assert.match(
    source,
    /async function requiredStep[\s\S]*?const entry = await step[\s\S]*?if \(!entry\.ok\)[\s\S]*?throw new EvidenceValidationError/,
  );
});

function fakeDriverSource({
  exitCode,
  writeEvidence,
  nestedCollision = false,
  outputText = "",
  writeDesktopEvidence = false,
}) {
  return `
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
const args = process.argv.slice(2);
const outIndex = args.indexOf("--out");
const out = outIndex >= 0 ? args[outIndex + 1] : ".";
mkdirSync(out, { recursive: true });
${writeEvidence ? `writeFileSync(path.join(out, "report.json"), "{}\\n");
writeFileSync(path.join(out, "account-status.json"), "{}\\n");
writeFileSync(path.join(out, "account-view.txt"), "configured invalid\\n");
writeFileSync(path.join(out, "diagnostic-export.json"), "{}\\n");
${writeDesktopEvidence ? `writeFileSync(path.join(out, "desktop-after-launch.png"), "fake png\\n");
writeFileSync(path.join(out, "desktop-after-drive.png"), "fake png\\n");` : ""}
${nestedCollision ? `mkdirSync(path.join(out, "nested"), { recursive: true });
writeFileSync(path.join(out, "nested", "account-status.json"), "nested collision\\n");` : ""}` : ""}
${outputText ? `console.log(${JSON.stringify(outputText)});` : ""}
process.exit(${exitCode});
`;
}

function fakeClientSource() {
  return `
import net from "node:net";
const port = Number(process.argv[2]);
const server = net.createServer();
server.listen(port, "127.0.0.1");
process.on("SIGTERM", () => server.close(() => process.exit(0)));
setInterval(() => {}, 1000);
`;
}

function earlyExitClientSource() {
  return `
setTimeout(() => process.exit(23), 1000);
`;
}

function orphaningClientParentSource() {
  return `
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
const [childScript, port, childPidPath] = process.argv.slice(2);
const child = spawn(process.execPath, [childScript, port], {
  detached: true,
  stdio: "ignore",
  windowsHide: true,
});
writeFileSync(childPidPath, String(child.pid));
child.unref();
setInterval(() => {}, 1000);
`;
}

function readRunnerLog(root) {
  const bytes = readFileSync(path.join(root, "run.log"));
  return bytes[0] === 0xff && bytes[1] === 0xfe
    ? bytes.subarray(2).toString("utf16le")
    : bytes.toString("utf8");
}

const runnerRootSentinelName = ".dataminer-client-ui-e2e-root";
const runnerRootSentinelContent = "dataminer-client-ui-e2e-root-v1\n";

function runPowerShellHarness({
  exitCode,
  writeEvidence,
  nestedCollision = false,
  portOverride,
  debugPortOverride,
  prepopulateDesktopShots = false,
  skipDesktopCapture = false,
  startupDelaySeconds = 0,
  outputText = "",
  writeDesktopEvidence = false,
  appDataEntries = {},
  staleRunEntries = {},
  rootSentinelMode = "auto",
  exeMode = "persistent",
  lockStaleEvidence = false,
}) {
  const fixtureRoot = mkdtempSync(path.join(tmpdir(), "dm-client-e2e-runner-"));
  const toolDir = path.join(fixtureRoot, "tools");
  const root = path.join(fixtureRoot, "run");
  const bundle = path.join(fixtureRoot, "e2e-bundle.zip");
  const localAppData = path.join(fixtureRoot, "local-app-data");
  const appDataRoot = path.join(localAppData, "com.diogeneslab.dataminer");
  mkdirSync(toolDir, { recursive: true });
  mkdirSync(localAppData, { recursive: true });
  if (prepopulateDesktopShots) {
    mkdirSync(root, { recursive: true });
    writeFileSync(path.join(root, "desktop-after-launch.png"), "stale launch\n");
    writeFileSync(path.join(root, "desktop-after-drive.png"), "stale drive\n");
  }
  for (const [relativePath, content] of Object.entries(staleRunEntries)) {
    const target = path.join(root, relativePath);
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, content);
  }
  let locker = null;
  if (lockStaleEvidence) {
    mkdirSync(root, { recursive: true });
    const lockedPath = path.join(root, "locked-stale.log");
    const readyPath = path.join(fixtureRoot, "lock-ready");
    writeFileSync(lockedPath, "locked stale evidence\n");
    locker = spawn("powershell.exe", [
      "-NoProfile",
      "-Command",
      `$stream = [IO.File]::Open('${lockedPath}', 'Open', 'ReadWrite', 'None'); `
        + `[IO.File]::WriteAllText('${readyPath}', 'ready'); `
        + "Start-Sleep -Seconds 30; $stream.Dispose()",
    ], { stdio: "ignore" });
    const deadline = Date.now() + 5_000;
    while (!existsSync(readyPath) && Date.now() < deadline) {
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25);
    }
    assert.equal(existsSync(readyPath), true, "locked evidence fixture did not become ready");
  }
  if (existsSync(root)) {
    const hasExistingEntries = readdirSync(root).length > 0;
    if (rootSentinelMode === "valid" || (rootSentinelMode === "auto" && hasExistingEntries)) {
      writeFileSync(path.join(root, runnerRootSentinelName), runnerRootSentinelContent);
    } else if (rootSentinelMode === "invalid") {
      writeFileSync(path.join(root, runnerRootSentinelName), "not-the-e2e-sentinel\n");
    }
  }
  for (const [relativePath, content] of Object.entries(appDataEntries)) {
    const target = path.join(appDataRoot, relativePath);
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, content);
  }
  copyFileSync(mockPath, path.join(toolDir, "mock-app-api.mjs"));
  writeFileSync(
    path.join(toolDir, "cdp-drive.mjs"),
    fakeDriverSource({
      exitCode,
      writeEvidence,
      nestedCollision,
      outputText,
      writeDesktopEvidence,
    }),
  );
  const port = portOverride ?? (45_000 + Math.floor(Math.random() * 1_000));
  const debugPort = debugPortOverride ?? (46_000 + Math.floor(Math.random() * 1_000));
  const clientPath = path.join(fixtureRoot, "fake-client.mjs");
  writeFileSync(clientPath, fakeClientSource());
  const earlyExitClientPath = path.join(fixtureRoot, "early-exit-client.mjs");
  writeFileSync(earlyExitClientPath, earlyExitClientSource());
  const orphaningClientPath = path.join(fixtureRoot, "orphaning-client.mjs");
  const orphanPidPath = path.join(fixtureRoot, "orphan-child.pid");
  writeFileSync(orphaningClientPath, orphaningClientParentSource());
  const brokenExecutable = path.join(fixtureRoot, "broken-client.exe");
  if (exeMode === "start-fail") {
    writeFileSync(brokenExecutable, "this is not a Windows executable\n");
  }
  const executable = exeMode === "missing"
    ? path.join(fixtureRoot, "missing-client.exe")
    : exeMode === "start-fail"
      ? brokenExecutable
      : exeMode === "persistent" || exeMode === "orphan-child" || exeMode === "early-exit"
        ? process.execPath
        : path.join(process.env.WINDIR ?? "C:\\Windows", "System32", "where.exe");
  const powershellArgs = [
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
    runnerPath,
    "-Exe",
    executable,
    "-Root",
    root,
    "-ToolDir",
    toolDir,
    "-Node",
    process.execPath,
    "-Bundle",
    bundle,
    "-Port",
    String(port),
    "-DebugPort",
    String(debugPort),
    "-CleanSlateDelaySeconds",
    "0",
    "-StartupDelaySeconds",
    String(startupDelaySeconds),
    "-BundleDelaySeconds",
    "0",
  ];
  if (exeMode === "persistent") {
    powershellArgs.push("-ExeArguments", `\"${clientPath}\" ${debugPort}`);
  } else if (exeMode === "early-exit") {
    powershellArgs.push("-ExeArguments", `\"${earlyExitClientPath}\"`);
  } else if (exeMode === "orphan-child") {
    powershellArgs.push(
      "-ExeArguments",
      `\"${orphaningClientPath}\" \"${clientPath}\" ${debugPort} \"${orphanPidPath}\"`,
    );
  }
  if (Object.keys(appDataEntries).length > 0) {
    powershellArgs.push("-AppDataRoot", appDataRoot);
  }
  if (skipDesktopCapture) powershellArgs.push("-SkipDesktopCapture");
  const result = spawnSync(
    "powershell.exe",
    powershellArgs,
    {
      encoding: "utf8",
      timeout: 30_000,
      env: { ...process.env, LOCALAPPDATA: localAppData },
    },
  );
  return { result, root, bundle, port, debugPort, appDataRoot, locker, orphanPidPath };
}

function allFiles(root) {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const fullPath = path.join(root, entry.name);
    return entry.isDirectory() ? allFiles(fullPath) : [fullPath];
  });
}

async function canBind(port) {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.once("error", () => resolve(false));
    server.listen(port, "127.0.0.1", () => server.close(() => resolve(true)));
  });
}

windowsTest("PowerShell runner preserves driver failure after cleanup and evidence bundling", () => {
  const { result, root, bundle } = runPowerShellHarness({ exitCode: 7, writeEvidence: true });

  assert.notEqual(result.status, 0, "a failed driver must fail the release E2E");
  assert.equal(readRunnerLog(root).includes("DRIVER_EXIT 7"), true);
  assert.equal(readRunnerLog(root).includes("E2E_FAILED"), true);
  assert.equal(readFileSync(bundle).length > 0, true, "failure evidence must still be bundled");
  assert.throws(() => readFileSync(path.join(root, "DONE")), /ENOENT/);
});

windowsTest("PowerShell runner records an early client exit as a run failure", () => {
  const { result, root } = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    startupDelaySeconds: 0,
    exeMode: "early-exit",
  });
  const log = readRunnerLog(root);

  assert.notEqual(result.status, 0);
  assert.match(log, /CLIENT_EXITED_EARLY code=23/);
  assert.match(log, /client_exited_early_23/);
  assert.doesNotMatch(log, /cdp_port_not_listening/);
});

windowsTest("PowerShell runner preserves relative paths when nested evidence shares a basename", () => {
  const { root } = runPowerShellHarness({ exitCode: 7, writeEvidence: true, nestedCollision: true });

  assert.equal(readFileSync(path.join(root, "_bundle", "account-status.json"), "utf8"), "{}\n");
  assert.equal(
    readFileSync(path.join(root, "_bundle", "nested", "account-status.json"), "utf8"),
    "nested collision\n",
  );
});

windowsTest("PowerShell runner rejects secret text outside the four core artifacts and never archives it", () => {
  const { result, root } = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    outputText: maliciousUrl,
  });
  const output = `${result.stdout}\n${result.stderr}`;
  const log = readRunnerLog(root);
  const bundleFiles = allFiles(path.join(root, "_bundle"));

  assert.notEqual(result.status, 0);
  assert.match(log, /SENSITIVE_EVIDENCE_REJECTED/);
  assert.doesNotMatch(output, new RegExp(maliciousMarker));
  assert.doesNotMatch(log, new RegExp(maliciousMarker));
  for (const file of bundleFiles.filter((file) => path.extname(file).toLowerCase() !== ".png")) {
    assert.doesNotMatch(readFileSync(file, "utf8"), new RegExp(maliciousMarker));
  }
});

windowsTest("PowerShell runner preserves deterministic appdata-relative paths without basename collisions", () => {
  const { root } = runPowerShellHarness({
    exitCode: 7,
    writeEvidence: true,
    appDataEntries: {
      "diagnostics/a/same.log": "first\n",
      "diagnostics/b/same.log": "second\n",
      "diagnostics/b/other.json": "{}\n",
    },
  });

  assert.equal(
    readFileSync(path.join(root, "appdata-logs", "diagnostics", "a", "same.log"), "utf8"),
    "first\n",
  );
  assert.equal(
    readFileSync(path.join(root, "appdata-logs", "diagnostics", "b", "same.log"), "utf8"),
    "second\n",
  );
});

windowsTest("PowerShell runner removes all stale run evidence before creating fresh logs and bundle", () => {
  const staleMarker = "stale-evidence-from-previous-run";
  const { root } = runPowerShellHarness({
    exitCode: 7,
    writeEvidence: true,
    staleRunEntries: {
      "run.log": staleMarker,
      "driver.log": staleMarker,
      "appdata-logs/old.json": staleMarker,
      "nested/old.txt": staleMarker,
      "_bundle/old.txt": staleMarker,
    },
  });

  assert.doesNotMatch(readRunnerLog(root), new RegExp(staleMarker));
  assert.equal(
    readFileSync(path.join(root, runnerRootSentinelName), "utf8"),
    runnerRootSentinelContent,
  );
  for (const file of allFiles(path.join(root, "_bundle"))) {
    if (path.extname(file).toLowerCase() === ".png") continue;
    assert.doesNotMatch(readFileSync(file, "utf8"), new RegExp(staleMarker));
  }
});

for (const rootSentinelMode of ["missing", "invalid"]) {
  windowsTest(`PowerShell runner refuses a non-empty root with a ${rootSentinelMode} sentinel without changing user bytes`, () => {
    const originalBytes = Buffer.from([0, 1, 2, 3, 0xfe, 0xff, 10]);
    const run = runPowerShellHarness({
      exitCode: 0,
      writeEvidence: true,
      staleRunEntries: { "user-owned.bin": originalBytes },
      rootSentinelMode,
    });
    const output = `${run.result.stdout}\n${run.result.stderr}`;

    assert.notEqual(run.result.status, 0);
    assert.match(output, /UNSAFE_RUN_ROOT_REFUSED/);
    assert.deepEqual(readFileSync(path.join(run.root, "user-owned.bin")), originalBytes);
    assert.doesNotMatch(output, /MOCK_READY/);
  });
}

windowsTest("PowerShell runner fails before starting services when stale evidence cannot be deleted", (t) => {
  const run = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    lockStaleEvidence: true,
  });
  t.after(() => run.locker?.kill());
  const output = `${run.result.stdout}\n${run.result.stderr}`;

  assert.notEqual(run.result.status, 0);
  assert.match(output, /PREVIOUS_EVIDENCE_CLEANUP_FAILED/);
  assert.doesNotMatch(output, /MOCK_READY/);
});

test("PowerShell runner contains no broad process-name cleanup", () => {
  const source = readFileSync(runnerPath, "utf8");

  assert.doesNotMatch(source, /Get-Process\s+(?:dataminer-client|msedgewebview2)/i);
  assert.doesNotMatch(source, /Get-CimInstance[\s\S]{0,200}mock-app-api/i);
  assert.doesNotMatch(source, /Stop-Process\s+-Force/i);
});

windowsTest("PowerShell runner removes stale desktop shots before checking required evidence", () => {
  const { result, root } = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    prepopulateDesktopShots: true,
    skipDesktopCapture: true,
  });
  const log = readRunnerLog(root);

  assert.notEqual(result.status, 0);
  assert.match(log, /EVIDENCE_MISSING desktop-after-launch\.png/);
  assert.match(log, /EVIDENCE_MISSING desktop-after-drive\.png/);
  assert.throws(() => readFileSync(path.join(root, "desktop-after-launch.png")), /ENOENT/);
  assert.throws(() => readFileSync(path.join(root, "desktop-after-drive.png")), /ENOENT/);
});

windowsTest("PowerShell runner fails closed when the mock port is already occupied", async (t) => {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const { result, root } = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    portOverride: address.port,
  });

  assert.notEqual(result.status, 0);
  assert.match(readRunnerLog(root), /PORT_IN_USE/);
  assert.doesNotMatch(readRunnerLog(root), /CLIENT_STARTED/);
});

windowsTest("PowerShell runner fails closed when the CDP port is already occupied", async (t) => {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const { result, root } = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    debugPortOverride: address.port,
  });

  assert.notEqual(result.status, 0);
  assert.match(readRunnerLog(root), /PORT_IN_USE cdp/);
  assert.doesNotMatch(readRunnerLog(root), /CLIENT_STARTED/);
});

for (const exeMode of ["missing", "start-fail"]) {
  windowsTest(`PowerShell runner releases its mock after ${exeMode} client startup failure`, async (t) => {
    const run = runPowerShellHarness({
      exitCode: 0,
      writeEvidence: false,
      exeMode,
    });
    const log = readRunnerLog(run.root);
    const pid = Number(/MOCK_READY True pid=(\d+)/.exec(log)?.[1]);
    t.after(() => {
      if (Number.isInteger(pid)) {
        spawnSync("powershell.exe", [
          "-NoProfile",
          "-Command",
          `Stop-Process -Id ${pid} -Force -ErrorAction SilentlyContinue`,
        ]);
      }
    });

    assert.notEqual(run.result.status, 0);
    assert.ok(Number.isInteger(pid), log);
    assert.equal(await canBind(run.port), true, "the tracked mock port must be released");
    const processProbe = spawnSync("powershell.exe", [
      "-NoProfile",
      "-Command",
      `if (Get-Process -Id ${pid} -ErrorAction SilentlyContinue) { exit 1 }`,
    ]);
    assert.equal(processProbe.status, 0, "the tracked mock PID must be reaped");
  });
}

windowsTest("PowerShell runner succeeds with a live client, writes DONE, and reaps both owned ports", async () => {
  const run = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    writeDesktopEvidence: true,
    skipDesktopCapture: true,
    exeMode: "persistent",
    startupDelaySeconds: 1,
  });

  assert.equal(run.result.status, 0, readRunnerLog(run.root));
  assert.match(readFileSync(path.join(run.root, "DONE"), "utf8"), /E2E_COMPLETE/);
  assert.equal(
    readFileSync(path.join(run.root, runnerRootSentinelName), "utf8"),
    runnerRootSentinelContent,
  );
  assert.equal(await canBind(run.port), true, "mock port must be released after success");
  assert.equal(await canBind(run.debugPort), true, "client debug port must be released after success");
});

windowsTest("PowerShell runner fails closed when a tracked client leaves its CDP child listening", async (t) => {
  const run = runPowerShellHarness({
    exitCode: 0,
    writeEvidence: true,
    writeDesktopEvidence: true,
    skipDesktopCapture: true,
    exeMode: "orphan-child",
    startupDelaySeconds: 1,
  });
  const childPid = Number(readFileSync(run.orphanPidPath, "utf8"));
  t.after(() => {
    if (Number.isInteger(childPid)) {
      spawnSync("powershell.exe", [
        "-NoProfile",
        "-Command",
        `Stop-Process -Id ${childPid} -Force -ErrorAction SilentlyContinue`,
      ]);
    }
  });

  assert.notEqual(run.result.status, 0, readRunnerLog(run.root));
  assert.match(readRunnerLog(run.root), /PORT_RELEASE_FAILED cdp/);
  assert.throws(() => readFileSync(path.join(run.root, "DONE")), /ENOENT/);
  assert.equal(await canBind(run.debugPort), false, "the orphan fixture must still hold CDP");
});

function verifyTargetList(targets) {
  const root = mkdtempSync(path.join(tmpdir(), "dm-client-e2e-targets-"));
  const targetPath = path.join(root, "targets.json");
  writeFileSync(targetPath, JSON.stringify(targets));
  return spawnSync(
    process.execPath,
    [driverPath, "--verify-target-list", targetPath, "--out", root],
    { encoding: "utf8", timeout: 3_000 },
  );
}

test("driver rejects an ambiguous CDP target list instead of selecting the first main page", () => {
  const result = verifyTargetList([
    { type: "page", title: "DataMiner", url: "tauri://localhost/", webSocketDebuggerUrl: "ws://one" },
    { type: "page", title: "DataMiner 2", url: "tauri://localhost/history", webSocketDebuggerUrl: "ws://two" },
  ]);

  assert.notEqual(result.status, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /exactly one main CDP page/);
});

windowsTest("PowerShell runner fails when the driver omits required report and diagnostic evidence", () => {
  const { result, root, bundle } = runPowerShellHarness({ exitCode: 0, writeEvidence: false });

  assert.notEqual(result.status, 0, "missing evidence must fail the release E2E");
  assert.match(readRunnerLog(root), /EVIDENCE_MISSING/);
  assert.equal(readFileSync(bundle).length > 0, true, "the remaining evidence must still be bundled");
  assert.throws(() => readFileSync(path.join(root, "DONE")), /ENOENT/);
});
