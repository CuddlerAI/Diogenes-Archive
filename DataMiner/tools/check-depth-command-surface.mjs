import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const formal = ["depth_lab_launch_cp2077", "depth_lab_preflight_cp2077"];
const local = [
  "depth_lab_preflight_cp2077_discovery", "depth_lab_start_cp2077_discovery",
  "depth_lab_stop_cp2077_discovery", "cp2077_trial_get_status",
  "cp2077_trial_prepare_and_launch", "cp2077_trial_start_recording",
  "cp2077_trial_stop_recording", "cp2077_trial_open_output",
];
const attributes = String.raw`((?:\s*#\[[^\]]*\])*)\s*`;

function withoutComments(source) {
  return source.replace(/"(?:\\.|[^"\\])*"|\/\/[^\n]*|\/\*[\s\S]*?\*\//g,
    (part) => part.startsWith('"') ? part : part.replace(/[^\n]/g, " "));
}

// Deliberately bounded to the Rust cfg forms used by these declarations. New
// syntax requires an explicit checker update; unknown cfg never means enabled.
function cfgValue(expression, features) {
  const tokens = expression.match(/"[^"\n]*"|[A-Za-z_][A-Za-z_0-9]*|[(),=]|\S/g) ?? [];
  let next = 0;
  const expect = (value) => {
    if (tokens[next++] !== value) throw new Error(`unsupported cfg expression: ${expression}`);
  };
  function parse() {
    const token = tokens[next++];
    if (token === "feature") {
      expect("=");
      const name = tokens[next++];
      if (!/^"[a-z0-9-]+"$/.test(name ?? "")) throw new Error(`unsupported cfg feature: ${expression}`);
      return features.has(JSON.parse(name));
    }
    if (["all", "any", "not"].includes(token)) {
      expect("(");
      const values = [];
      while (tokens[next] !== ")") {
        values.push(parse());
        if (tokens[next] !== ",") break;
        next += 1;
      }
      expect(")");
      if (token === "not" && values.length !== 1) throw new Error(`unsupported cfg not: ${expression}`);
      return token === "all" ? values.every(Boolean) : token === "any" ? values.some(Boolean) : !values[0];
    }
    if (token === "test" || token === "debug_assertions") return false;
    if (token === "windows") return true;
    throw new Error(`unsupported cfg predicate: ${expression}`);
  }
  const result = parse();
  if (next !== tokens.length) throw new Error(`unsupported trailing cfg: ${expression}`);
  return result;
}

function enabled(attrs, features) {
  const conditions = [...attrs.matchAll(/#\[cfg\(([^\]]*)\)\]/g)];
  if (attrs.includes("cfg_attr")) throw new Error("cfg_attr on command surface requires explicit review");
  return conditions.map((match) => cfgValue(match[1], features)).every(Boolean);
}

export function readDepthCommandSources(root) {
  const src = path.join(root, "apps/client/src-tauri/src");
  return Object.fromEntries([
    ["lib", "lib.rs"], ["modules", "commands/mod.rs"],
    ["depth", "commands/depth_lab.rs"], ["trial", "commands/cp2077_trial.rs"],
    ["early", "services/early_launch.rs"],
    ["service", "services/depth_lab.rs"],
  ].map(([key, file]) => [key, readFileSync(path.join(src, file), "utf8").replace(/\r\n/g, "\n")]));
}

export function checkDepthCommandSurface(sources) {
  const source = Object.fromEntries(Object.entries(sources).map(([key, value]) => [key, withoutComments(value)]));
  const importGroups = [...source.lib.matchAll(new RegExp(`${attributes}use\\s+commands::(depth_lab|cp2077_trial)::\\{([^}]+)\\};`, "g"))];
  const handlers = [...source.lib.matchAll(/\.invoke_handler\(tauri::generate_handler!\[([\s\S]*?)\]\)/g)];
  if (handlers.length !== 1) throw new Error("expected one actual invoke_handler generate_handler registration");
  const entries = [...handlers[0][1].matchAll(new RegExp(`${attributes}([a-z_][a-z_0-9]*)\\s*,`, "g"))];
  const modes = [
    ["default", []], ["delivered-catalog/production", ["depth-lab"]],
    ["default-dev", ["dev-catalog"]], ["all-games-dev", ["depth-lab", "dev-catalog", "local-catalog-only"]],
  ];
  for (const [mode, featureNames] of modes) {
    const features = new Set(featureNames);
    for (const name of [...formal, ...local]) {
      const module = name.startsWith("cp2077_trial_") ? "cp2077_trial" : "depth_lab";
      const moduleMatches = [...source.modules.matchAll(new RegExp(`${attributes}pub\\s+mod\\s+${module}\\s*;`, "g"))];
      const definitions = [...source[module === "depth_lab" ? "depth" : "trial"].matchAll(new RegExp(`${attributes}pub\\s+(?:async\\s+)?fn\\s+${name}\\s*\\(`, "g"))];
      const imports = importGroups.filter((group) => group[2] === module && new RegExp(`\\b${name}\\b`).test(group[3]));
      const registrations = entries.filter((entry) => entry[2] === name);
      for (const [label, matches] of [["module", moduleMatches], ["definition", definitions], ["import", imports], ["handler", registrations]]) {
        if (matches.length !== 1) throw new Error(`${name}: expected exactly one ${label}, found ${matches.length}`);
      }
      if (!definitions[0][1].includes("#[tauri::command]")) throw new Error(`${name}: definition lacks tauri command annotation`);
      const expected = features.has("depth-lab") && (formal.includes(name) || features.has("dev-catalog"));
      const stages = {
        definition: enabled(moduleMatches[0][1], features) && enabled(definitions[0][1], features),
        import: enabled(imports[0][1], features),
        handler: enabled(registrations[0][1], features),
      };
      for (const [label, actual] of Object.entries(stages)) {
        if (actual !== expected) throw new Error(`${mode}: ${name} ${label} is ${actual ? "exposed" : "missing"}; expected ${expected ? "reachable" : "excluded"} with cfg(test)=false`);
      }
    }
  }
  // Every allowlisted target is promoted (CP2077 since 2026-09-18). What must survive is that
  // formal early launch resolves only allowlisted targets and pins their exact executable.
  const resolver = /fn resolve_formal_early_launch_target\([\s\S]*?\n\}/.exec(source.service)?.[0] ?? "";
  if (!resolver.includes("EarlyLaunchTarget::from_formal_game_id(game_id)")
      || !resolver.includes("target.verify_executable_identity(game_id, executable_sha256)?;")) {
    throw new Error("formal early-launch must resolve only allowlisted targets and verify their exact executable");
  }
  return modes.map(([name]) => name);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = process.argv[2] ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const modes = checkDepthCommandSurface(readDepthCommandSources(root));
  process.stdout.write(`PASS: Depth command definition/import/invoke registration in ${modes.join(", ")} (cfg(test)=false)\n`);
}
