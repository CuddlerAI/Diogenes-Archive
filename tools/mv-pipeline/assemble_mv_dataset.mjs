#!/usr/bin/env node
// EXPERIMENTAL / method-validation only —— 见 model_research/research/20 号文。
// 把 data-demo-converter 的 ready/<session_id>/ 产出组装成 docs/12 §4.2 形态的
// per-session tar shard + 索引。shard 不跨 session（撤销演练前提），tar 为自实现
// ustar：条目按 chunk 序、mtime=0、uid/gid=0 —— 同一输入重跑字节一致。
// 用法：node assemble_mv_dataset.mjs --session-dir <ready/sid> --out <dir>
//        --split train|test [--shard-bytes 1073741824]
// 产出：<out>/<split>/<sid>/shard-NNN.tar 与 <out>/index/sessions/<sid>/
//        {session-index.json, samples.jsonl}（外加 framepairs/metadata 原样拷贝）。
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  if (i === -1 || i + 1 >= process.argv.length) {
    if (fallback !== undefined) return fallback;
    console.error(`missing ${name}`);
    process.exit(2);
  }
  return process.argv[i + 1];
}

const sessionDir = path.resolve(arg("--session-dir"));
const outDir = path.resolve(arg("--out"));
const split = arg("--split");
const shardBytesTarget = Number(arg("--shard-bytes", "1073741824"));
if (!["train", "test"].includes(split)) {
  console.error(`--split must be train|test, got ${split}`);
  process.exit(2);
}

// converter ready 布局：<ready>/<game_slug>/<session_id>/{metadata.json,framepairs.jsonl,cNNNN/...}
function findSessionRoot(root) {
  const hits = [];
  for (const slug of fs.readdirSync(root, { withFileTypes: true })) {
    if (!slug.isDirectory()) continue;
    const slugDir = path.join(root, slug.name);
    for (const sid of fs.readdirSync(slugDir, { withFileTypes: true })) {
      if (!sid.isDirectory()) continue;
      if (fs.existsSync(path.join(slugDir, sid.name, "metadata.json"))) {
        hits.push({ gameSlug: slug.name, sessionId: sid.name, dir: path.join(slugDir, sid.name) });
      }
    }
  }
  if (hits.length !== 1) {
    console.error(`expected exactly one <game_slug>/<session_id> under ${root}, found ${hits.length}`);
    process.exit(2);
  }
  return hits[0];
}

const { gameSlug, sessionId, dir: chunkRoot } = findSessionRoot(sessionDir);
const chunkDirs = fs
  .readdirSync(chunkRoot, { withFileTypes: true })
  .filter((e) => e.isDirectory() && /^c\d{4,}$/.test(e.name))
  .map((e) => e.name)
  .sort();
if (chunkDirs.length === 0) {
  console.error(`no chunk dirs under ${chunkRoot}`);
  process.exit(2);
}

// ---- 确定性 ustar ----
function tarHeader(name, size) {
  const b = Buffer.alloc(512);
  if (Buffer.byteLength(name) > 100) throw new Error(`tar name too long: ${name}`);
  b.write(name, 0, "utf8");
  b.write("0000644\0", 100); // mode
  b.write("0000000\0", 108); // uid
  b.write("0000000\0", 116); // gid
  b.write(size.toString(8).padStart(11, "0") + "\0", 124);
  b.write("00000000000\0", 136); // mtime=0
  b.write("        ", 148); // checksum 占位（8 空格）
  b.write("0", 156); // typeflag regular
  b.write("ustar\0", 257);
  b.write("00", 263);
  let sum = 0;
  for (const byte of b) sum += byte;
  b.write(sum.toString(8).padStart(6, "0") + "\0 ", 148);
  return b;
}

class ShardWriter {
  constructor(shardPath) {
    this.path = shardPath;
    this.fd = fs.openSync(shardPath, "w");
    this.hash = createHash("sha256");
    this.bytes = 0;
    this.entries = [];
  }
  add(entryName, filePath) {
    const data = fs.readFileSync(filePath);
    const header = tarHeader(entryName, data.length);
    const pad = Buffer.alloc((512 - (data.length % 512)) % 512);
    for (const buf of [header, data, pad]) {
      fs.writeSync(this.fd, buf);
      this.hash.update(buf);
      this.bytes += buf.length;
    }
    this.entries.push({ name: entryName, bytes: data.length });
  }
  close() {
    const eof = Buffer.alloc(1024);
    fs.writeSync(this.fd, eof);
    this.hash.update(eof);
    this.bytes += eof.length;
    fs.closeSync(this.fd);
    return { path: this.path, bytes: this.bytes, sha256: this.hash.digest("hex"), entries: this.entries };
  }
}

const shardDir = path.join(outDir, split, sessionId);
const indexDir = path.join(outDir, "index", "sessions", sessionId);
fs.mkdirSync(shardDir, { recursive: true });
fs.mkdirSync(indexDir, { recursive: true });

const CHUNK_FILES = ["p0.mp4", "p0.jsonl", "meta.json"];
const shards = [];
const samples = [];
let writer = null;
let shardIndex = 0;

for (const chunk of chunkDirs) {
  const chunkPath = path.join(chunkRoot, chunk);
  const meta = JSON.parse(fs.readFileSync(path.join(chunkPath, "meta.json"), "utf8"));
  const chunkBytes = CHUNK_FILES.reduce(
    (sum, f) => sum + fs.statSync(path.join(chunkPath, f)).size,
    0,
  );
  if (writer && writer.bytes + chunkBytes > shardBytesTarget) {
    shards.push(writer.close());
    writer = null;
  }
  if (!writer) {
    const name = `shard-${String(shardIndex).padStart(3, "0")}.tar`;
    writer = new ShardWriter(path.join(shardDir, name));
    shardIndex += 1;
  }
  for (const f of CHUNK_FILES) {
    // rocket-science 键约定：<session_id>_cNNNN.<file>
    writer.add(`${sessionId}_${chunk}.${f}`, path.join(chunkPath, f));
  }
  samples.push({
    session_id: sessionId,
    chunk,
    split,
    shard: path.basename(writer.path),
    game_slug: gameSlug,
    bytes: chunkBytes,
    is_trainable: false,
    method_validation_only: true,
    meta: {
      frames: meta.frames ?? meta.frame_count ?? null,
      rights: meta.rights ?? null,
      non_trainable: meta.non_trainable ?? null,
    },
  });
}
if (writer) shards.push(writer.close());

fs.writeFileSync(
  path.join(indexDir, "samples.jsonl"),
  samples.map((s) => JSON.stringify(s)).join("\n") + "\n",
);
for (const f of ["metadata.json", "framepairs.jsonl"]) {
  const src = path.join(chunkRoot, f);
  if (fs.existsSync(src)) fs.copyFileSync(src, path.join(indexDir, f));
}
for (const f of ["metadata.jsonl", "_dataset_stats.json", "DATASHEET.md"]) {
  const src = path.join(sessionDir, f);
  if (fs.existsSync(src)) fs.copyFileSync(src, path.join(indexDir, `converter-${f}`));
}

const sessionIndex = {
  schema: "dm-mv-session-index-1",
  session_id: sessionId,
  game_slug: gameSlug,
  split,
  is_trainable: false,
  method_validation_only: true,
  chunk_count: chunkDirs.length,
  shards: shards.map((s) => ({
    name: path.basename(s.path),
    bytes: s.bytes,
    sha256: s.sha256,
    entry_count: s.entries.length,
  })),
};
fs.writeFileSync(path.join(indexDir, "session-index.json"), JSON.stringify(sessionIndex, null, 2) + "\n");
console.log(
  `assembled session=${sessionId} split=${split} chunks=${chunkDirs.length} shards=${shards.length} bytes=${shards.reduce((a, s) => a + s.bytes, 0)}`,
);
