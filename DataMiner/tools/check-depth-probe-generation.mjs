import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const POLICY_PATH = "config/depth-probe/runtime-policy.json";
// Keep this list deliberately small and repository-owned. Its job is only to force a human/AI
// impact declaration when source-controlled inputs can change the probe bytes; it does not guess
// whether that change affects D3D11, D3D12, both, or neither.
const PROBE_INPUT_PREFIXES = [
  "tools/depth-lab-probe/",
  "tools/depth-lab/",
  "crates/present-identity/",
  ".cargo/",
];
const PROBE_INPUT_FILES = new Set([
  "Cargo.toml",
  "Cargo.lock",
  "rust-toolchain",
  "rust-toolchain.toml",
]);
const REPOSITORY_PROBE_INPUT_FILES = new Set([
  ".github/workflows/depth-probe-release.yml",
]);
const IMPACTS = new Set(["none", "d3d11", "d3d12", "both"]);
const POLICY_KEYS = [
  "schemaVersion",
  "runtimeVersion",
  "changeImpact",
  "changeReason",
  "d3d11Generation",
  "d3d12Generation",
].sort();

function assertPolicy(policy, label) {
  if (typeof policy !== "object" || policy === null || Array.isArray(policy)) {
    throw new Error(`${label} must be a JSON object`);
  }
  const keys = Object.keys(policy).sort();
  if (JSON.stringify(keys) !== JSON.stringify(POLICY_KEYS)) {
    throw new Error(`${label} has unexpected fields: ${keys.join(", ")}`);
  }
  if (policy.schemaVersion !== "dm-depth-probe-generation-policy-v1") {
    throw new Error(`${label} has an unsupported schemaVersion`);
  }
  if (typeof policy.runtimeVersion !== "string" || policy.runtimeVersion.trim() === "") {
    throw new Error(`${label} runtimeVersion must not be empty`);
  }
  if (!IMPACTS.has(policy.changeImpact)) {
    throw new Error(`${label} changeImpact must be none, d3d11, d3d12, or both`);
  }
  if (typeof policy.changeReason !== "string" || policy.changeReason.trim() === "") {
    throw new Error(`${label} changeReason must not be empty`);
  }
  for (const field of ["d3d11Generation", "d3d12Generation"]) {
    if (!Number.isSafeInteger(policy[field]) || policy[field] <= 0) {
      throw new Error(`${label} ${field} must be a positive safe integer`);
    }
  }
}

export function validateTransition(previous, current, { probeChanged, policyChanged }) {
  assertPolicy(current, "current Depth probe policy");
  if (previous === null) {
    if (
      current.changeImpact !== "both" ||
      current.d3d11Generation !== 1 ||
      current.d3d12Generation !== 1
    ) {
      throw new Error("the initial Depth probe policy must declare both at generation 1/1");
    }
    return;
  }
  assertPolicy(previous, "previous Depth probe policy");
  if (probeChanged && !policyChanged) {
    throw new Error(
      "Depth probe inputs changed without an explicit runtime-policy.json declaration",
    );
  }
  if (!policyChanged) {
    return;
  }

  const expectedD3d11 =
    previous.d3d11Generation + (current.changeImpact === "d3d11" || current.changeImpact === "both" ? 1 : 0);
  const expectedD3d12 =
    previous.d3d12Generation + (current.changeImpact === "d3d12" || current.changeImpact === "both" ? 1 : 0);
  if (
    current.d3d11Generation !== expectedD3d11 ||
    current.d3d12Generation !== expectedD3d12
  ) {
    throw new Error(
      `changeImpact=${current.changeImpact} requires generations ${expectedD3d11}/${expectedD3d12}, ` +
        `got ${current.d3d11Generation}/${current.d3d12Generation}`,
    );
  }
}

function git(args) {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function policyAt(ref) {
  try {
    return JSON.parse(
      execFileSync("git", ["show", `${ref}:DataMiner/${POLICY_PATH}`], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      }),
    );
  } catch {
    return null;
  }
}

export function isProbeInput(path) {
  const normalized = path.replaceAll("\\", "/");
  if (REPOSITORY_PROBE_INPUT_FILES.has(normalized)) {
    return true;
  }
  const relative = normalized.startsWith("DataMiner/")
    ? normalized.slice("DataMiner/".length)
    : normalized;
  return (
    PROBE_INPUT_FILES.has(relative) ||
    PROBE_INPUT_PREFIXES.some((prefix) => relative.startsWith(prefix))
  );
}

function main() {
  const argument = process.argv.find((value) => value.startsWith("--base-ref="));
  const baseRef = argument?.slice("--base-ref=".length) || process.env.DM_DEPTH_PROBE_BASE_REF;
  if (!baseRef) {
    throw new Error("pass --base-ref=<git ref> or set DM_DEPTH_PROBE_BASE_REF");
  }
  const current = JSON.parse(readFileSync(POLICY_PATH, "utf8"));
  const previous = policyAt(baseRef);
  const changed = git(["diff", "--name-only", `${baseRef}...HEAD`])
    .split(/\r?\n/u)
    .filter(Boolean);
  validateTransition(previous, current, {
    probeChanged: changed.some(isProbeInput),
    policyChanged: changed.includes(`DataMiner/${POLICY_PATH}`) || changed.includes(POLICY_PATH),
  });
  process.stdout.write(
    `Depth probe declaration accepted: impact=${current.changeImpact}, ` +
      `D3D11=${current.d3d11Generation}, D3D12=${current.d3d12Generation}\n`,
  );
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`Depth probe generation check failed: ${error.message}\n`);
    process.exitCode = 1;
  }
}
