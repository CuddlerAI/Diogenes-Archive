import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { checkDepthCommandSurface, readDepthCommandSources } from "../check-depth-command-surface.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const current = () => readDepthCommandSources(root);
const devGate = '#[cfg(all(feature = "depth-lab", any(feature = "dev-catalog", test)))]';

test("actual invoke definitions/imports/handlers cover the four production feature sets", () => {
  assert.deepEqual(checkDepthCommandSurface(current()), ["default", "delivered-catalog/production", "default-dev", "all-games-dev"]);
});

for (const name of ["depth_lab_launch_cp2077", "depth_lab_preflight_cp2077"]) {
  test(`${name}: detects the original dev-only definition even though cfg(test) exposes it`, () => {
    const sources = current();
    sources.depth = sources.depth.replace(`#[tauri::command]\npub fn ${name}(`,
      `#[cfg(any(feature = "dev-catalog", test))]\n#[tauri::command]\npub fn ${name}(`);
    assert.throws(() => checkDepthCommandSurface(sources), /delivered-catalog\/production: .* definition is missing/);
  });
  test(`${name}: detects a dev-only generate_handler registration`, () => {
    const sources = current();
    sources.lib = sources.lib.replace(new RegExp(`#\\[cfg\\(feature = "depth-lab"\\)\\]\\s+${name},`), `${devGate}\n            ${name},`);
    assert.throws(() => checkDepthCommandSurface(sources), /delivered-catalog\/production: .* handler is missing/);
  });
}

test("a dev-only import group cannot be rescued by a reachable handler", () => {
  const sources = current();
  sources.lib = sources.lib.replace(/#\[cfg\(feature = "depth-lab"\)\]\s*use commands::depth_lab::\{/,
    `${devGate}\nuse commands::depth_lab::{`);
  assert.throws(() => checkDepthCommandSurface(sources), /delivered-catalog\/production: .* import is missing/);
});

test("removing the parent depth module gate leaks definitions into default builds", () => {
  const sources = current();
  sources.modules = sources.modules.replace(/#\[cfg\(feature = "depth-lab"\)\]\s*(pub mod depth_lab;)/, "$1");
  assert.throws(() => checkDepthCommandSurface(sources), /default: .* definition is exposed/);
});

for (const name of ["depth_lab_start_cp2077_discovery", "cp2077_trial_prepare_and_launch"]) {
  test(`${name}: local-only invocation must stay excluded from Delivered Catalog`, () => {
    const sources = current();
    sources.lib = sources.lib.replace(new RegExp(`#\\[cfg\\(all\\(feature = "depth-lab", any\\(feature = "dev-catalog", test\\)\\)\\)\\]\\s+${name},`), `#[cfg(feature = "depth-lab")]\n            ${name},`);
    assert.throws(() => checkDepthCommandSurface(sources), /delivered-catalog\/production: .* handler is exposed/);
  });
}

test("a commented-out handler does not count as registered", () => {
  const sources = current();
  sources.lib = sources.lib.replace(/(\s+)(depth_lab_launch_cp2077,)(?=\s*#\[cfg)/, "$1// $2");
  assert.throws(() => checkDepthCommandSurface(sources), /expected exactly one handler, found 0/);
});

test("an unsupported cfg predicate fails closed", () => {
  const sources = current();
  sources.depth = sources.depth.replace("#[tauri::command]\npub fn depth_lab_launch_cp2077(", '#[cfg(unreviewed_flag)]\n#[tauri::command]\npub fn depth_lab_launch_cp2077(');
  assert.throws(() => checkDepthCommandSurface(sources), /unsupported cfg predicate/);
});

for (const removed of [
  "target.verify_executable_identity(game_id, executable_sha256)?;",
  "EarlyLaunchTarget::from_formal_game_id(game_id)",
]) {
  test(`formal early launch cannot drop ${removed}`, () => {
    const sources = current();
    assert.ok(sources.service.includes(removed));
    sources.service = sources.service.replace(removed, "");
    assert.throws(() => checkDepthCommandSurface(sources), /verify their exact executable/);
  });
}
