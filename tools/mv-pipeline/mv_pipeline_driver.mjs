// VM-side pipeline driver: per-session download → convert → verify → assemble → upload → clean.
// Runs detached; appends JSON lines to STATUS; uploads logs at the end.
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const ROOT = "C:/dm/mv2";
const CURL = "C:/Windows/System32/curl.exe";
const CONVERTER = "C:/dm/cargo-target/release/dataminer-data-demo.exe";
// 恢复策略：Alt(18) 保留为控制键——实测 18 个 session 各一次、全落段尾 1 秒内、
// 且零抬起，是切窗口收尾噪声而非 ping_wheel；Ctrl(17) 释放回 move.prone。
const CONTROL_VKS = "18,119,121";
const ASSEMBLER = `${ROOT}/assemble_mv_dataset.mjs`;
const STATUS = `${ROOT}/status.log`;
const manifest = JSON.parse(fs.readFileSync(`${ROOT}/mv-manifest.json`, "utf8"));

function log(obj) {
  const line = JSON.stringify({ t: new Date().toISOString(), ...obj });
  fs.appendFileSync(STATUS, line + "\n");
  console.log(line);
}
function rmrf(p) { fs.rmSync(p, { recursive: true, force: true }); }
function sha256File(p) {
  const h = createHash("sha256");
  const fd = fs.openSync(p, "r");
  const buf = Buffer.alloc(4 * 1024 * 1024);
  let n;
  while ((n = fs.readSync(fd, buf)) > 0) h.update(buf.subarray(0, n));
  fs.closeSync(fd);
  return h.digest("hex");
}
function hashTree(root) {
  const out = {};
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else out[path.relative(root, p).replaceAll("\\", "/")] = sha256File(p);
    }
  };
  walk(root);
  return out;
}
function curlDownload(sid, gets) {
  const cfg = [`--parallel`, `--parallel-max 12`, `--retry 4`, `--retry-all-errors`, `--create-dirs`, `-sS`, `--fail`].join("\n") + "\n" +
    gets.map((g) => `url = "${g.url}"\noutput = "${ROOT}/raw/${sid}/${g.rel}"`).join("\n") + "\n";
  const cfgPath = `${ROOT}/dl-${sid}.cfg`;
  fs.writeFileSync(cfgPath, cfg);
  const r = spawnSync(CURL, ["--config", cfgPath], { stdio: ["ignore", "pipe", "pipe"], maxBuffer: 64 * 1024 * 1024 });
  fs.rmSync(cfgPath);
  if (r.status !== 0) throw new Error(`curl download failed: ${r.stderr}`);
  const bad = gets.filter((g) => {
    try { return fs.statSync(`${ROOT}/raw/${sid}/${g.rel}`).size !== g.size; } catch { return true; }
  });
  if (bad.length) throw new Error(`size mismatch on ${bad.length} files, first: ${bad[0].rel}`);
}
function curlPut(url, file) {
  for (let attempt = 1; attempt <= 4; attempt++) {
    const r = spawnSync(CURL, ["-sS", "--fail", "-X", "PUT", "-T", file, url], { stdio: ["ignore", "pipe", "pipe"] });
    if (r.status === 0) return;
    if (attempt === 4) throw new Error(`PUT failed for ${file}: ${r.stderr}`);
  }
}
function runConverter(args) {
  const r = spawnSync(CONVERTER, args, { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 });
  return { status: r.status, stdout: (r.stdout || "") + (r.stderr || "") };
}

fs.mkdirSync(`${ROOT}/asm`, { recursive: true });
let determinismChecked = false;
const summary = [];
for (const s of manifest.sessions) {
  const sid = s.session_id;
  const result = { session: sid, split: s.split, expect_fail: s.expect_fail };
  const t0 = Date.now();
  try {
    log({ phase: "download", session: sid, files: s.gets.length });
    curlDownload(sid, s.gets);

    log({ phase: "reproject", session: sid });
    const rp = runConverter(["reproject", "--session", `${ROOT}/raw/${sid}`, "--control-vks", CONTROL_VKS]);
    if (!s.expect_fail) {
      if (rp.status !== 0) throw new Error(`reproject failed: ${rp.stdout.slice(-400)}`);
      try {
        const rep = JSON.parse(rp.stdout.trim().split("\n").filter(Boolean).pop());
        result.reprojection = { freed_events: rep.freed_events_total, rows_changed: rep.rows_changed_total,
                                 segments: rep.segments.length, control_vks: rep.control_vks };
      } catch { result.reprojection = { parse_error: rp.stdout.slice(-200) }; }
    }

    log({ phase: "convert", session: sid });
    const conv = runConverter(["convert", "--session", `${ROOT}/raw/${sid}`, "--output", `${ROOT}/ds/${sid}`, "--profile", "local-debug", "--control-vks", CONTROL_VKS]);
    result.convert_exit = conv.status;
    result.convert_tail = conv.stdout.split("\n").filter(Boolean).slice(-3);
    if (s.expect_fail) {
      result.failed_as_expected = conv.status !== 0;
      if (conv.status === 0) result.anomaly = "faulted session converted successfully; NOT uploaded";
    } else {
      if (conv.status !== 0) throw new Error(`convert failed: ${result.convert_tail.join(" | ")}`);
      const ver = runConverter(["verify", "--dataset", `${ROOT}/ds/${sid}`]);
      result.verify_exit = ver.status;
      if (ver.status !== 0) throw new Error(`verify failed: ${ver.stdout.slice(-500)}`);

      if (!determinismChecked) {
        log({ phase: "determinism", session: sid });
        const conv2 = runConverter(["convert", "--session", `${ROOT}/raw/${sid}`, "--output", `${ROOT}/ds2/${sid}`, "--profile", "local-debug", "--control-vks", CONTROL_VKS]);
        if (conv2.status === 0) {
          const a = hashTree(`${ROOT}/ds/${sid}`);
          const b = hashTree(`${ROOT}/ds2/${sid}`);
          result.determinism_identical = JSON.stringify(a) === JSON.stringify(b);
          result.determinism_files = Object.keys(a).length;
        } else {
          result.determinism_identical = "second run failed";
        }
        rmrf(`${ROOT}/ds2`);
        determinismChecked = true;
      }

      log({ phase: "assemble", session: sid });
      const asm = spawnSync("C:/dm/node/node.exe", [ASSEMBLER, "--session-dir", `${ROOT}/ds/${sid}`, "--out", `${ROOT}/asm`, "--split", s.split], { encoding: "utf8" });
      if (asm.status !== 0) throw new Error(`assemble failed: ${asm.stderr}`);
      result.assemble = (asm.stdout || "").trim();

      log({ phase: "upload", session: sid });
      const shardDir = `${ROOT}/asm/${s.split}/${sid}`;
      const shards = fs.readdirSync(shardDir).filter((f) => f.endsWith(".tar")).sort();
      result.shards = [];
      for (const shard of shards) {
        const key = `${s.split}/${sid}/${shard}`;
        const url = s.puts[key];
        if (!url) throw new Error(`no presigned PUT for ${key} (shard count exceeded presigned range)`);
        const file = `${shardDir}/${shard}`;
        curlPut(url, file);
        result.shards.push({ shard, bytes: fs.statSync(file).size });
      }
      const idxDir = `${ROOT}/asm/index/sessions/${sid}`;
      for (const f of fs.readdirSync(idxDir)) {
        const key = `index/sessions/${sid}/${f}`;
        if (s.puts[key]) curlPut(s.puts[key], `${idxDir}/${f}`);
      }
      rmrf(shardDir);
    }
    result.ok = true;
  } catch (err) {
    result.ok = false;
    result.error = String(err && err.message ? err.message : err).slice(0, 800);
  }
  result.seconds = Math.round((Date.now() - t0) / 1000);
  rmrf(`${ROOT}/raw/${sid}`);
  rmrf(`${ROOT}/ds/${sid}`);
  // per-session log upload (best effort)
  try {
    const logKey = `logs/${sid}.log`;
    if (s.puts[logKey]) {
      const tmp = `${ROOT}/one-${sid}.json`;
      fs.writeFileSync(tmp, JSON.stringify(result, null, 2));
      curlPut(s.puts[logKey], tmp);
      fs.rmSync(tmp);
    }
  } catch {}
  summary.push(result);
  log({ phase: "done", session: sid, ok: result.ok, seconds: result.seconds });
}

fs.writeFileSync(`${ROOT}/final-summary.json`, JSON.stringify({ finished: new Date().toISOString(), summary }, null, 2));
try {
  curlPut(manifest.global_puts["logs/final-summary.json"], `${ROOT}/final-summary.json`);
  curlPut(manifest.global_puts["logs/pipeline-status.log"], STATUS);
} catch (err) {
  log({ phase: "final-upload-failed", error: String(err) });
}
log({ phase: "ALL_DONE", ok_count: summary.filter((r) => r.ok).length, total: summary.length });
