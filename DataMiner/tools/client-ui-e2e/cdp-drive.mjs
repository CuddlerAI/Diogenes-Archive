#!/usr/bin/env node
// cdp-drive.mjs — drives the real DataMiner client through the Chrome DevTools Protocol.
//
// The client is a Tauri 2 app, so its UI is a WebView2 page. Launching the exe with
// `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>` exposes that page
// over CDP, which lets an E2E walk the *shipping binary* — real Rust backend, real IPC,
// real catalog — instead of a browser-only mock of the frontend. Screenshots come from
// `Page.captureScreenshot`; because the window is `decorations: false` with a custom
// titlebar, that image is the entire app surface, not just a content pane.
//
// Zero dependencies: Node 22 ships a global WebSocket.
//
// Usage: node cdp-drive.mjs --port 9222 --out <dir>

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { basename, join } from "node:path";

const args = process.argv.slice(2);
const arg = (n, d) => {
  const i = args.indexOf(`--${n}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : d;
};
const PORT = Number(arg("port", "9222"));
const OUT = arg("out", "./out");
mkdirSync(OUT, { recursive: true });

const EXPECTED_ACCOUNT_FAILURE_REASON = "account base URL must be https";
const FORBIDDEN_ACCOUNT_MARKER = "must-not-be-exported";

class EvidenceValidationError extends Error {}

function invariant(condition, message) {
  if (!condition) throw new EvidenceValidationError(message);
}

function publicError(error, fallback = "driver execution failed") {
  return error instanceof EvidenceValidationError ? error.message : fallback;
}

function verifyNoAccountSecret(value, forbiddenAccountUrl, artifactName) {
  invariant(
    typeof forbiddenAccountUrl === "string" && forbiddenAccountUrl.length > 0,
    "the invalid account URL is unavailable for evidence validation",
  );
  const serialized = typeof value === "string" ? value : JSON.stringify(value);
  invariant(!serialized.includes(forbiddenAccountUrl), `${artifactName} contains the forbidden account URL`);
  invariant(!serialized.includes(FORBIDDEN_ACCOUNT_MARKER), `${artifactName} contains the forbidden account marker`);
}

export function verifyInvalidAccountEvidence(status, viewText, forbiddenAccountUrl) {
  verifyNoAccountSecret(status, forbiddenAccountUrl, "account status evidence");
  verifyNoAccountSecret(viewText, forbiddenAccountUrl, "account view evidence");
  invariant(status && typeof status === "object", "account_get_status did not return an object");
  invariant(status.schemaVersion === "dm-account-status-2", "account_get_status must use dm-account-status-2");
  invariant(status.availability?.kind === "invalid", "account must be configured invalid, not unconfigured");
  invariant(status.availability.code === "account_base_url_invalid", "account invalid status must report account_base_url_invalid");
  invariant(typeof status.availability.message === "string" && status.availability.message.length > 0, "account invalid status must retain a safe user message");
  invariant(viewText.includes("账号服务配置无效"), "account UI must render the configured-invalid state");
  invariant(!viewText.includes("账号服务未配置"), "account UI must not render invalid configuration as unconfigured");
}

export function verifyStartupDiagnosticEvidence(diagnostic, forbiddenAccountUrl, startedUnixMs) {
  verifyNoAccountSecret(diagnostic, forbiddenAccountUrl, "diagnostic export");
  invariant(diagnostic?.schema_version === "dm-diagnostic-export-1", "diagnostic export schema is not dm-diagnostic-export-1");
  invariant(Array.isArray(diagnostic.events), "diagnostic export events are missing");
  invariant(Number.isSafeInteger(startedUnixMs) && startedUnixMs > 0, "the current run start timestamp is invalid");
  invariant(
    Number.isSafeInteger(diagnostic.created_unix_ms) && diagnostic.created_unix_ms >= startedUnixMs,
    "diagnostic export timestamp is invalid for the current run",
  );
  const startupFailures = diagnostic.events.filter(
    (event) => event?.kind === "service_startup_degraded"
      && event?.data?.service === "account"
      && event?.data?.code === "account_base_url_invalid",
  );
  invariant(startupFailures.length > 0, "diagnostic export is missing the account service_startup_degraded event");
  const startupFailure = startupFailures.find(
    (event) => Number.isSafeInteger(event.timestamp_unix_ms)
      && event.timestamp_unix_ms >= startedUnixMs
      && event.timestamp_unix_ms <= diagnostic.created_unix_ms,
  );
  invariant(startupFailure, "diagnostic export has no account startup failure within the current export snapshot");
  invariant(
    startupFailure.data.reason === EXPECTED_ACCOUNT_FAILURE_REASON,
    "diagnostic export does not contain the safe real account startup reason",
  );
}

function verifyEvidenceDirectory(directory, forbiddenAccountUrl, startedUnixMs) {
  const status = JSON.parse(readFileSync(join(directory, "account-status.json"), "utf8"));
  const viewText = readFileSync(join(directory, "account-view.txt"), "utf8");
  const diagnostic = JSON.parse(readFileSync(join(directory, "diagnostic-export.json"), "utf8"));
  const savedReport = JSON.parse(readFileSync(join(directory, "report.json"), "utf8"));
  verifyInvalidAccountEvidence(status, viewText, forbiddenAccountUrl);
  verifyStartupDiagnosticEvidence(diagnostic, forbiddenAccountUrl, startedUnixMs);
  verifyNoAccountSecret(savedReport, forbiddenAccountUrl, "driver report evidence");
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const report = { startedAt: new Date().toISOString(), steps: [], console: [], commands: [], shots: [] };
let shotIndex = 0;

class Cdp {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.id = 0;
    this.pending = new Map();
    this.ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(JSON.stringify(msg.error)));
        else resolve(msg.result);
      } else if (msg.method) {
        this.onEvent(msg.method, msg.params);
      }
    });
  }
  onEvent() {}
  ready() {
    return new Promise((resolve, reject) => {
      if (this.ws.readyState === 1) return resolve();
      this.ws.addEventListener("open", () => resolve(), { once: true });
      this.ws.addEventListener("error", (e) => reject(new Error(`ws error: ${e?.message ?? e}`)), { once: true });
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`timeout: ${method}`));
        }
      }, 30000);
    });
  }
}

async function findTarget() {
  for (let i = 0; i < 60; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/json/list`);
      const list = await res.json();
      const mainPages = mainPageTargets(list);
      if (mainPages.length > 1) {
        throw new EvidenceValidationError("expected exactly one main CDP page");
      }
      if (mainPages.length === 1) return { main: mainPages[0], all: list };
    } catch (error) {
      if (error instanceof EvidenceValidationError) throw error;
      /* the webview has not opened its debug port yet */
    }
    await sleep(1000);
  }
  throw new Error("no CDP page target appeared within 60s");
}

function mainPageTargets(targets) {
  invariant(Array.isArray(targets), "CDP target list is invalid");
  return targets.filter(
    (target) => target?.type === "page"
      && !/overlay/i.test(target.url ?? "")
      && !/overlay/i.test(target.title ?? ""),
  );
}

export function selectMainTarget(targets) {
  const mainPages = mainPageTargets(targets);
  invariant(mainPages.length === 1, "expected exactly one main CDP page");
  return mainPages[0];
}

let cdp;
async function evalJs(expression, awaitPromise = false) {
  const r = await cdp.send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise,
    userGesture: true,
  });
  if (r.exceptionDetails) {
    return { error: r.exceptionDetails.exception?.description ?? r.exceptionDetails.text };
  }
  return { value: r.result.value };
}

async function shot(name) {
  try {
    const { data } = await cdp.send("Page.captureScreenshot", { format: "png" });
    const file = `${String(++shotIndex).padStart(2, "0")}-${name}.png`;
    writeFileSync(join(OUT, file), Buffer.from(data, "base64"));
    report.shots.push(file);
    return file;
  } catch (err) {
    report.shots.push(`FAILED:${name}:${err.message}`);
    return null;
  }
}

/** Compact, reviewable outline of what is actually on screen right now. */
const OUTLINE_JS = `(() => {
  const t = (el) => (el.innerText || el.textContent || "").replace(/\\s+/g, " ").trim().slice(0, 160);
  const vis = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const out = {};
  out.title = document.title;
  out.headings = [...document.querySelectorAll("h1,h2,h3")].filter(vis).map(t).filter(Boolean);
  out.buttons = [...document.querySelectorAll("button")].filter(vis).map((b) => ({
    label: t(b) || b.getAttribute("aria-label") || "",
    disabled: b.disabled === true,
    cls: b.className || "",
  }));
  out.selects = [...document.querySelectorAll("select")].filter(vis).map((s) => ({
    id: s.id, disabled: s.disabled, value: s.value,
    options: [...s.options].map((o) => ({ v: o.value, label: (o.textContent||"").trim(), disabled: o.disabled })),
  }));
  out.inputs = [...document.querySelectorAll("input,textarea")].filter(vis).map((i) => ({
    type: i.type, name: i.name || i.id || "", placeholder: i.placeholder || "",
    disabled: i.disabled, checked: i.type === "checkbox" ? i.checked : undefined,
  }));
  out.alerts = [...document.querySelectorAll('[role="alert"],.error,.notification,.profile-meta')].filter(vis).map(t).filter(Boolean);
  out.dialogs = [...document.querySelectorAll('[role="dialog"]')].filter(vis).map(t);
  const root = document.querySelector("#root");
  out.text = root ? (root.innerText || "").replace(/\\n{3,}/g, "\\n\\n").slice(0, 6000) : "";
  return out;
})()`;

async function step(name, fn, { required = false } = {}) {
  const entry = { name, at: new Date().toISOString(), required };
  try {
    entry.result = (await fn()) ?? null;
    entry.ok = true;
  } catch (err) {
    entry.ok = false;
    entry.error = publicError(err);
  }
  report.steps.push(entry);
  return entry;
}

async function requiredStep(name, fn) {
  const entry = await step(name, fn, { required: true });
  if (!entry.ok) {
    // A required evidence failure is a safety boundary, not merely a red row in the
    // report. In particular, an old binary that silently falls back to production
    // must stop here before the driver can reach any account mutation.
    throw new EvidenceValidationError(`required evidence step failed: ${name}`);
  }
  return entry;
}

/** Click the visible button whose text matches, via a real DOM click. */
async function clickButton(text) {
  const r = await evalJs(`(() => {
    const b = [...document.querySelectorAll("button")].find((x) =>
      ((x.innerText || x.textContent || "").replace(/\\s+/g, " ").trim().includes(${JSON.stringify(text)})));
    if (!b) return "not-found";
    if (b.disabled) return "disabled";
    b.click();
    return "clicked";
  })()`);
  return r.value ?? r.error;
}

// Read-only or lab-safe commands. Deliberately excluded, with reasons:
//   collector_choose_capture_storage_location  -> opens a modal native folder picker (would hang the run)
//   collector_open_session_directory / _data_model_directory -> spawns explorer.exe on the desktop
//   collector_delete_session                   -> destructive
//   account_logout / account_change_password   -> ordered separately inside the account flow
const SWEEP = [
  ["collector_get_p0_status", {}],
  ["collector_get_snapshot", {}],
  ["collector_get_capture_storage_status", {}],
  ["collector_list_games", {}],
  ["collector_list_session_history", {}],
  ["collector_get_overlay_snapshot", {}],
  ["account_get_status", {}],
  ["account_get_terms", {}],
  ["account_get_consent_notice", {}],
  ["account_get_device_enrollment_status", {}],
  // Argument shapes are not uniform across the surface: most collector commands take a
  // `request` envelope, but `view_scale_begin_session` takes a bare `kind` and
  // `collector_set_overlay_visible` a bare `visible`. Each entry below matches its command.
  ["client_get_device_credential_status", { request: { environment: "production" } }],
  ["client_get_update_status", {}],
  ["view_scale_get_status", {}],
  ["view_scale_list_history", {}],
];

async function invokeCmd(name, argsObj) {
  const r = await evalJs(
    `(async () => {
       try {
         const v = await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(name)}, ${JSON.stringify(argsObj)});
         return { ok: true, value: v };
       } catch (e) {
         return { ok: false, error: typeof e === "string" ? e : JSON.stringify(e) };
       }
     })()`,
    true,
  );
  return r.value ?? { ok: false, error: r.error };
}

function summarize(value) {
  const s = typeof value === "string" ? value : JSON.stringify(value);
  return s === undefined ? "undefined" : s.slice(0, 700);
}

async function main() {
  const { main, all } = await findTarget();
  report.targets = all.map((t) => ({ type: t.type, title: t.title, url: t.url }));
  report.mainTarget = { title: main.title, url: main.url };

  cdp = new Cdp(main.webSocketDebuggerUrl);
  cdp.onEvent = (method, params) => {
    if (method === "Runtime.consoleAPICalled") {
      report.console.push({
        level: params.type,
        text: (params.args ?? []).map((a) => a.value ?? a.description ?? a.type).join(" ").slice(0, 500),
      });
    } else if (method === "Runtime.exceptionThrown") {
      report.console.push({
        level: "exception",
        text: (params.exceptionDetails?.exception?.description ?? params.exceptionDetails?.text ?? "").slice(0, 800),
      });
    } else if (method === "Log.entryAdded") {
      report.console.push({ level: `log:${params.entry.level}`, text: (params.entry.text ?? "").slice(0, 500) });
    }
  };
  await cdp.ready();
  await cdp.send("Runtime.enable");
  await cdp.send("Log.enable");
  await cdp.send("Page.enable");

  await requiredStep("wait-for-mount", async () => {
    for (let i = 0; i < 40; i++) {
      const r = await evalJs(`document.querySelector("#root") && document.querySelector("#root").children.length > 0`);
      if (r.value === true) return "mounted";
      await sleep(500);
    }
    throw new Error("#root never mounted");
  });
  // The snapshot poll runs at 500ms; give the first catalog/game load time to land.
  await sleep(4000);

  await step("capture-view", async () => {
    const o = await evalJs(OUTLINE_JS);
    await shot("capture-view");
    return o.value ?? o.error;
  });

  await step("command-sweep", async () => {
    for (const [name, a] of SWEEP) {
      const res = await invokeCmd(name, a);
      report.commands.push({ name, ok: res.ok, detail: summarize(res.ok ? res.value : res.error) });
    }
    return `${report.commands.filter((c) => c.ok).length}/${report.commands.length} ok`;
  });

  await step("scan-windows", async () => {
    const clicked = await clickButton("扫描窗口");
    await sleep(2500);
    const o = await evalJs(OUTLINE_JS);
    await shot("capture-after-scan");
    return { clicked, alerts: o.value?.alerts, text: (o.value?.text ?? "").slice(0, 1200) };
  });

  await step("start-without-target", async () => {
    // With no bound window this must refuse cleanly; the *message* is the thing under review.
    const res = await invokeCmd("collector_start_session", { request: { requestId: "e2e-start-probe" } });
    await sleep(1200);
    const o = await evalJs(OUTLINE_JS);
    await shot("start-refused");
    return { invoke: summarize(res.ok ? res.value : res.error), alerts: o.value?.alerts };
  });

  await step("history-view", async () => {
    const clicked = await clickButton("历史");
    await sleep(2500);
    const o = await evalJs(OUTLINE_JS);
    await shot("history-view");
    return { clicked, outline: o.value ?? o.error };
  });

  await requiredStep("account-invalid-configuration", async () => {
    const clicked = await clickButton("账号");
    await sleep(3000);
    const o = await evalJs(OUTLINE_JS);
    const screenshot = await shot("account-view");
    const status = await invokeCmd("account_get_status", {});
    invariant(clicked === "clicked", "account navigation button was not clicked");
    invariant(!o.error && o.value, "account outline failed");
    invariant(screenshot, "account invalid-state screenshot is missing");
    invariant(status.ok, "account_get_status failed");
    verifyInvalidAccountEvidence(
      status.value,
      o.value.text ?? "",
      process.env.DATAMINER_ACCOUNT_API_BASE_URL,
    );
    writeFileSync(join(OUT, "account-status.json"), `${JSON.stringify(status.value, null, 2)}\n`);
    writeFileSync(join(OUT, "account-view.txt"), `${o.value.text ?? ""}\n`);
    return {
      clicked,
      schemaVersion: status.value.schemaVersion,
      availability: status.value.availability,
      screenshot,
    };
  });

  await requiredStep("account-startup-diagnostic-export", async () => {
    const exported = await invokeCmd("collector_export_diagnostics", { sessionId: null });
    invariant(exported.ok, "collector_export_diagnostics failed");
    const metadata = exported.value;
    invariant(metadata && typeof metadata === "object", "diagnostic export metadata is missing");
    invariant(typeof metadata.fileName === "string", "diagnostic export fileName is missing");
    invariant(basename(metadata.fileName) === metadata.fileName && metadata.fileName.endsWith(".json"), "diagnostic export fileName is unsafe");
    invariant(typeof metadata.sha256 === "string" && /^[a-f0-9]{64}$/.test(metadata.sha256), "diagnostic export sha256 is invalid");
    const localAppData = process.env.LOCALAPPDATA;
    invariant(localAppData, "LOCALAPPDATA is unavailable");
    const exportPath = join(
      localAppData,
      "com.diogeneslab.dataminer",
      "diagnostics",
      "exports",
      metadata.fileName,
    );
    const bytes = readFileSync(exportPath);
    invariant(createHash("sha256").update(bytes).digest("hex") === metadata.sha256, "diagnostic export SHA-256 does not match IPC metadata");
    const diagnostic = JSON.parse(bytes.toString("utf8"));
    verifyStartupDiagnosticEvidence(
      diagnostic,
      process.env.DATAMINER_ACCOUNT_API_BASE_URL,
      Number(process.env.DATAMINER_E2E_STARTED_UNIX_MS),
    );
    writeFileSync(join(OUT, "diagnostic-export.json"), bytes);
    return {
      fileName: metadata.fileName,
      sha256: metadata.sha256,
      eventCount: metadata.eventCount,
      startupFailureReason: EXPECTED_ACCOUNT_FAILURE_REASON,
    };
  });

  await step("account-terms", async () => {
    const res = await invokeCmd("account_get_terms", {});
    return summarize(res.ok ? res.value : res.error);
  });

  // This executable mode deliberately validates F2/F3 with an invalid account URL. It is
  // read-only by construction: registration, verification, enrollment, logout and consent
  // mutations belong in a separate HTTPS-staging acceptance mode, after the /terms proof.

  await step("view-scale-calibration", async () => {
    const begin = await invokeCmd("view_scale_begin_session", { kind: "spot_check" });
    await sleep(1500);
    const o = await evalJs(OUTLINE_JS);
    await shot("view-scale");
    const cancel = await invokeCmd("view_scale_cancel_session", {});
    return {
      begin: summarize(begin.ok ? begin.value : begin.error),
      cancel: summarize(cancel.ok ? cancel.value : cancel.error),
      headings: o.value?.headings,
    };
  });

  await step("overlay-window", async () => {
    const on = await invokeCmd("collector_set_overlay_visible", { visible: true });
    await sleep(1500);
    const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
    const off = await invokeCmd("collector_set_overlay_visible", { visible: false });
    return {
      show: summarize(on.ok ? on.value : on.error),
      hide: summarize(off.ok ? off.value : off.error),
      targets: list.map((t) => t.title),
    };
  });

  await step("update-check", async () => {
    const before = await invokeCmd("client_get_update_status", {});
    const checked = await invokeCmd("client_check_for_update", {});
    await sleep(3000);
    const after = await invokeCmd("client_get_update_status", {});
    await shot("update-status");
    return {
      before: summarize(before.ok ? before.value : before.error),
      check: summarize(checked.ok ? checked.value : checked.error),
      after: summarize(after.ok ? after.value : after.error),
    };
  });

  await step("back-to-capture", async () => {
    const clicked = await clickButton("采集");
    await sleep(1500);
    await shot("final-capture-view");
    return clicked;
  });

  await step("layout-probe", async () => {
    // Narrow window: minWidth is 1040, so 1040x680 is the smallest supported viewport.
    const metrics = await evalJs(`({ w: innerWidth, h: innerHeight, sw: document.documentElement.scrollWidth, sh: document.documentElement.scrollHeight })`);
    return metrics.value ?? metrics.error;
  });

  report.finishedAt = new Date().toISOString();
  const requiredFailures = report.steps.filter((entry) => entry.required && !entry.ok);
  report.verdict = requiredFailures.length === 0
    ? { ok: true }
    : { ok: false, failedRequiredSteps: requiredFailures.map((entry) => entry.name) };
  verifyNoAccountSecret(
    report,
    process.env.DATAMINER_ACCOUNT_API_BASE_URL,
    "driver report evidence",
  );
  writeFileSync(join(OUT, "report.json"), JSON.stringify(report, null, 2));
  console.log(`E2E_REPORT_WRITTEN steps=${report.steps.length} shots=${report.shots.length}`);
  // An open WebSocket keeps Node's event loop alive forever, which strands the calling
  // PowerShell in a pipeline that never returns — the run then looks like it died right
  // after the driver started. Close it and leave deliberately.
  try {
    cdp.ws.close();
  } catch {
    /* already gone */
  }
  if (requiredFailures.length > 0) {
    console.error(`E2E_REQUIRED_STEPS_FAILED ${requiredFailures.map((entry) => entry.name).join(",")}`);
    process.exit(1);
  }
  process.exit(0);
}

const evidenceDirectory = arg("verify-evidence", null);
if (evidenceDirectory) {
  try {
    verifyEvidenceDirectory(
      evidenceDirectory,
      arg("forbidden-account-url", ""),
      Number(arg("started-unix-ms", "")),
    );
    console.log("E2E_EVIDENCE_VERIFIED");
    process.exit(0);
  } catch (err) {
    console.error(`E2E_EVIDENCE_INVALID ${publicError(err, "evidence could not be validated")}`);
    process.exit(1);
  }
}

const targetListPath = arg("verify-target-list", null);
if (targetListPath) {
  try {
    selectMainTarget(JSON.parse(readFileSync(targetListPath, "utf8")));
    console.log("E2E_TARGET_LIST_VERIFIED");
    process.exit(0);
  } catch (err) {
    console.error(`E2E_TARGET_LIST_INVALID ${publicError(err, "target list could not be validated")}`);
    process.exit(1);
  }
}

main().catch((err) => {
  const failure = publicError(err);
  report.fatal = failure;
  report.finishedAt = new Date().toISOString();
  try {
    let artifact = report;
    try {
      verifyNoAccountSecret(
        report,
        process.env.DATAMINER_ACCOUNT_API_BASE_URL,
        "driver report evidence",
      );
    } catch {
      artifact = {
        startedAt: report.startedAt,
        finishedAt: report.finishedAt,
        verdict: { ok: false, failedRequiredSteps: ["sensitive-evidence-validation"] },
        fatal: "sensitive evidence rejected",
      };
    }
    writeFileSync(join(OUT, "report.json"), JSON.stringify(artifact, null, 2));
  } catch {
    /* nothing left to do */
  }
  console.log(`E2E_FATAL ${failure}`);
  process.exit(1);
});
