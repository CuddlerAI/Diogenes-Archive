import assert from "node:assert/strict";
import test from "node:test";

import { isProbeInput, validateTransition } from "../check-depth-probe-generation.mjs";

function policy(impact, d3d11, d3d12) {
  return {
    schemaVersion: "dm-depth-probe-generation-policy-v1",
    runtimeVersion: "depth-2026.08.1",
    changeImpact: impact,
    changeReason: "reviewed change",
    d3d11Generation: d3d11,
    d3d12Generation: d3d12,
  };
}

test("impact declaration controls only the declared backend generation", () => {
  const previous = policy("none", 4, 9);
  assert.doesNotThrow(() =>
    validateTransition(previous, policy("none", 4, 9), {
      probeChanged: true,
      policyChanged: true,
    }),
  );
  assert.doesNotThrow(() =>
    validateTransition(previous, policy("d3d11", 5, 9), {
      probeChanged: true,
      policyChanged: true,
    }),
  );
  assert.doesNotThrow(() =>
    validateTransition(previous, policy("d3d12", 4, 10), {
      probeChanged: true,
      policyChanged: true,
    }),
  );
  assert.doesNotThrow(() =>
    validateTransition(previous, policy("both", 5, 10), {
      probeChanged: true,
      policyChanged: true,
    }),
  );
});

test("CI rejects undeclared probe changes and wrong increments", () => {
  const previous = policy("none", 4, 9);
  assert.throws(
    () =>
      validateTransition(previous, policy("none", 4, 9), {
        probeChanged: true,
        policyChanged: false,
      }),
    /without an explicit/u,
  );
  assert.throws(
    () =>
      validateTransition(previous, policy("d3d12", 5, 10), {
        probeChanged: true,
        policyChanged: true,
      }),
    /requires generations 4\/10/u,
  );
  assert.throws(
    () =>
      validateTransition(previous, policy("both", 6, 10), {
        probeChanged: true,
        policyChanged: true,
      }),
    /requires generations 5\/10/u,
  );
});

test("initial policy is exactly generation 1/1 and both", () => {
  assert.doesNotThrow(() =>
    validateTransition(null, policy("both", 1, 1), {
      probeChanged: true,
      policyChanged: true,
    }),
  );
  assert.throws(
    () =>
      validateTransition(null, policy("none", 1, 1), {
        probeChanged: false,
        policyChanged: true,
      }),
    /initial/u,
  );
});

test("probe input detection includes direct shared code and controlled build inputs", () => {
  for (const path of [
    "DataMiner/tools/depth-lab-probe/src/probe.rs",
    "DataMiner/tools/depth-lab/src/exr_out.rs",
    "DataMiner/crates/present-identity/src/lib.rs",
    "DataMiner/Cargo.toml",
    "DataMiner/Cargo.lock",
    "DataMiner/rust-toolchain.toml",
    "DataMiner/.cargo/config.toml",
    ".github/workflows/depth-probe-release.yml",
  ]) {
    assert.equal(isProbeInput(path), true, `${path} must require an impact declaration`);
  }

  for (const path of [
    "DataMiner/apps/client/src/App.tsx",
    "DataMiner/tools/New-DepthProbeArtifact.ps1",
    ".github/workflows/dataminer-release.yml",
    "README.md",
  ]) {
    assert.equal(isProbeInput(path), false, `${path} must not create declaration noise`);
  }
});
