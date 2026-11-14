#!/usr/bin/env node

import {
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep, win32 } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CAPABILITIES_PATH = join(ROOT, "config", "game-depth-capabilities.json");
const SHIPPED_PATH = join(ROOT, "config", "catalogs", "shipped-games.json");
const FINAL_CLASSIFICATIONS_PATH = join(ROOT, "config", "game-depth-final-classifications.json");
const RIGHTS_GATES_PATH = join(ROOT, "config", "game-depth-rights-gates.json");
const PERFORMANCE_CERTIFICATIONS_PATH = join(
  ROOT,
  "config",
  "depth-probe",
  "performance-certifications.json",
);
const MAFIA3_LOCAL_PROFILE_VALIDATOR_PATH = join(
  ROOT,
  "tools",
  "game-adaptation",
  "Test-Mafia3D6LocalProfileEvidence.ps1",
);
const ALLOWED_CAPABILITIES = new Set([
  "catalog_ready",
  "local_trial",
  "adaptation_blocked",
  "not_adaptable",
  "not_assessed",
]);
const ALLOWED_EVIDENCE_TIERS = new Set([
  "exact_build_runtime",
  "formal_catalog",
  "exact_build_local_trial",
  "exact_build_planning_only",
  "product_policy_survey",
  "product_survey_only",
  "catalog_only",
]);
const ALLOWED_GAME_KEYS = new Set([
  "gameId",
  "evidenceTier",
  "evidenceLocator",
  "rendererEvidence",
  "capability",
  "runtimeMode",
  "depthCaptureAvailable",
  "recordingReady",
  "historicalPerformancePairs",
  "blockers",
  "evidence",
]);
const ALLOWED_RENDERER_EVIDENCE_KEYS = new Set([
  "schema",
  "graphicsApis",
  "rendererMode",
  "apiEvidenceTier",
  "exactBuildIdentity",
  "evidence",
]);
const ALLOWED_EXACT_BUILD_IDENTITY_KEYS = new Set([
  "appId",
  "buildId",
  "executableBasename",
  "executableSha256",
]);
const ALLOWED_RENDERER_APIS = new Set(["d3d11", "d3d12", "vulkan", "unknown"]);
const RENDERER_API_ORDER = ["d3d11", "d3d12", "vulkan", "unknown"];
const ALLOWED_RENDERER_MODES = new Set(["single", "multi", "unknown"]);
const ALLOWED_API_EVIDENCE_TIERS = new Set([
  "exact_build_runtime_report",
  "exact_build_observation",
  "exact_build_local_trial",
  "product_survey",
  "unknown",
]);
const ALLOWED_TOP_LEVEL_KEYS = new Set([
  "$comment",
  "schemaVersion",
  "reviewedAt",
  "finalLedgerAudit",
  "games",
  "pendingOnboarding",
]);
const ALLOWED_FINAL_LEDGER_AUDIT_KEYS = new Set([
  "schema",
  "ledgerTotalRecords",
  "shippedRosterGames",
  "mappedLedgerRecords",
  "mappedShippedGames",
  "unmappedLedgerRecords",
  "unmappedShippedGames",
  "hardTerminalMappedRecords",
  "hardTerminalMappedGames",
  "provisionalMappedRecords",
  "provisionalMappedGames",
  "mappedRecords",
  "unmappedLedgerAppIds",
  "unmappedShippedGameIds",
]);
const ALLOWED_FINAL_LEDGER_MAPPING_KEYS = new Set([
  "appId",
  "gameIds",
  "classification",
  "finalForObservedBuild",
  "authorityMaturity",
  "provisionalReasons",
]);
const PRODUCT_ONLY_FINAL_EVIDENCE_KINDS = new Set([
  "official_store_disclosure",
  "official_store_taxonomy",
  "repository_survey",
]);
const ALLOWED_FINAL_AUTHORITY_MATURITY = new Set([
  "hard_terminal_for_observed_build",
  "provisional",
]);
const ALLOWED_PENDING_ONBOARDING_KEYS = new Set([
  "gameId",
  "displayName",
  "appId",
  "targetBuildId",
  "steamStateFlags",
  "catalogAuthored",
  "capability",
  "executableCandidate",
  "executableSha256",
  "graphicsApi",
  "runtimeEvidence",
  "localProfileEvidence",
  "blockers",
  "evidence",
]);
const MAFIA3_PENDING_GAME_ID = "mafia-iii-definitive-edition-steam";
const ALLOWED_PROVISIONAL_REASONS = new Set([
  "final_for_observed_build_false",
  "product_survey_or_taxonomy_only",
  "independent_repository_record_missing",
  "catalog_final_executable_identity_conflict",
]);
const EXACT_BUILD_API_EVIDENCE_TIERS = new Set([
  "exact_build_runtime_report",
  "exact_build_observation",
  "exact_build_local_trial",
]);
const MAFIA3_LOCAL_PROFILE_EVIDENCE_TEMPLATE = {
  schema: "dm-mafia3-d6-local-profile-evidence/v1",
  status: "pass",
  validator_sha256: `sha256:${fileSha256(MAFIA3_LOCAL_PROFILE_VALIDATOR_PATH)}`,
  proof_scope: "local_profile_bytes_only",
  profile_embeds_build_identity: false,
  external_install_gate_required: true,
  launcher: {
    basename: "Launcher.exe",
    sha256: "sha256:a1a8c67f2024fd7613fce513eb0cb339d1642fe95ec38b5ff04af20345f6dde4",
    bytes: 83773952,
    field_mapping_evidence:
      "official W3Launcher.VideoConfigFile.LoadVideoConfigFile read-only reflection",
  },
  input_profile: {
    basename: "gameinputmodule.pf",
    sha256: "sha256:009ca0dfb756a5f3c4e877ee057e79283a6fa595828f3d6cb93b8dc5cd66a313",
    bytes: 396612,
    structure: {
      xml_bytes: 396608,
      trailer_bytes: 4,
      control_set_count: 4,
      controller_type_2_logical_controls: 130,
      device_type_2_bound_tuples: 87,
      device_type_1_bound_tuples: 20,
      canonical_control_set_sha256:
        "sha256:3964e5ab41d1d399c8a5934dfb4b23c46313fdb9b025dbdb58076a8cf8267fa9",
      mouse_sensitivity: 0.5,
      invert_x: false,
      invert_y: false,
    },
    core_bindings: [
      ["UP", "profile_device_type_2", 17, 32],
      ["DOWN", "profile_device_type_2", 31, 32],
      ["LEFT", "profile_device_type_2", 30, 32],
      ["RIGHT", "profile_device_type_2", 32, 32],
      ["AIM_X", "profile_device_type_1", 300, 1],
      ["AIM_Y", "profile_device_type_1", 301, 1],
      ["RUN", "profile_device_type_2", 42, 32],
      ["SPRINT", "profile_device_type_2", 42, 64],
      ["CLIMB", "profile_device_type_2", 57, 32],
      ["STALK", "profile_device_type_2", 45, 32],
      ["COVER_ENTER", "profile_device_type_2", 29, 32],
      ["FIRE", "profile_device_type_1", 400, 32],
      ["ZOOM", "profile_device_type_1", 401, 32],
      ["RELOAD", "profile_device_type_2", 19, 32],
      ["ACTION", "profile_device_type_2", 18, 32],
      ["PICKUP_BODY", "profile_device_type_2", 21, 32],
      ["GENERIC_ACTION_1", "profile_device_type_2", 33, 32],
      ["WEAPON_GRENADE_AIM", "profile_device_type_2", 34, 32],
      ["HOLSTER", "profile_device_type_2", 35, 32],
      ["SWITCH_EXPLOSIVES", "profile_device_type_2", 44, 32],
      ["USE_MEDKIT", "profile_device_type_2", 46, 32],
      ["INTEL_VIEW_OPEN", "profile_device_type_2", 47, 32],
      ["MAP", "profile_device_type_2", 15, 32],
      ["MENU", "profile_device_type_2", 1, 32],
      ["CAR_UP", "profile_device_type_2", 17, 32],
      ["CAR_DOWN", "profile_device_type_2", 31, 32],
      ["CAR_LEFT", "profile_device_type_2", 30, 32],
      ["CAR_RIGHT", "profile_device_type_2", 32, 32],
      ["HANDBRAKE", "profile_device_type_2", 57, 32],
      ["CAR_FIRE", "profile_device_type_1", 400, 32],
      ["CAR_EXIT", "profile_device_type_2", 18, 32],
    ].map(([logical_control, device_kind, element, control_mode]) => ({
      logical_control,
      device_kind,
      element,
      control_mode,
      slot: "primary",
    })),
    source_logical_names_preserved: true,
    default_preset_confirmed: false,
  },
  video_config: {
    basename: "videoconfig.cfg",
    sha256: "sha256:1ba4ee58ac40d2fc3643c03dbe1d683b913276c1f0d9fd1a50461b6889d0e977",
    bytes: 23,
    fields: {
      position_x: 0,
      position_y: 0,
      resolution_x: 1920,
      resolution_y: 1080,
      is_fullscreen: false,
      autodetect_resolution: false,
      vsync_enabled: true,
      gpu_index: 0,
      gpu_name: "Default",
      display_index: 0,
      display_name: "Default",
      hdr_field_present: false,
      hdr_confirmed: false,
    },
  },
  catalog_authored: false,
  rgb_recording_ready: false,
  depth_capture_available: false,
  blockers: [
    "representative_single_player_runtime_evidence_missing",
    "input_profile_default_preset_confirmation_missing",
    "physical_input_mapping_and_semantic_authoring_missing",
    "hdr_or_sdr_confirmation_missing",
    "d7_rgb_runtime_evidence_missing",
    "depth_profile_missing",
    "camera_binding_missing",
  ],
};

function loadJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function evidencePathExists(root, evidence) {
  if (
    typeof evidence !== "string" ||
    evidence.length === 0 ||
    isAbsolute(evidence) ||
    evidence.split(/[\\/]/).includes("..")
  ) {
    return false;
  }

  try {
    const canonicalRoot = realpathSync(root);
    const canonicalCandidate = realpathSync(resolve(root, evidence));
    const relativePath = relative(canonicalRoot, canonicalCandidate);
    return (
      relativePath.length > 0 &&
      relativePath !== ".." &&
      !relativePath.startsWith(`..${sep}`) &&
      statSync(canonicalCandidate).isFile()
    );
  } catch {
    return false;
  }
}

function selfTestEvidencePaths() {
  const canonicalTemp = realpathSync(tmpdir());
  const fixtureRoot = mkdtempSync(join(canonicalTemp, "dm-depth-capability-root-"));
  const outsideRoot = mkdtempSync(join(canonicalTemp, "dm-depth-capability-outside-"));
  const junction = join(fixtureRoot, "escaped");
  try {
    mkdirSync(join(fixtureRoot, "evidence"));
    writeFileSync(join(fixtureRoot, "evidence", "inside.json"), "{}\n");
    writeFileSync(join(outsideRoot, "outside.json"), "{}\n");
    symlinkSync(outsideRoot, junction, "junction");
    if (!evidencePathExists(fixtureRoot, "evidence/inside.json")) {
      throw new Error("self-test rejected a repository-internal evidence file");
    }
    if (evidencePathExists(fixtureRoot, "evidence")) {
      throw new Error("self-test accepted a directory as evidence");
    }
    if (evidencePathExists(fixtureRoot, "escaped/outside.json")) {
      throw new Error("self-test accepted evidence through an escaping junction");
    }
  } finally {
    for (const path of [junction, fixtureRoot, outsideRoot]) {
      const resolved = resolve(path);
      const relativePath = relative(canonicalTemp, resolved);
      if (
        relativePath.length > 0 &&
        relativePath !== ".." &&
        !relativePath.startsWith(`..${sep}`)
      ) {
        rmSync(resolved, { recursive: true, force: true });
      }
    }
  }
}

function duplicateValues(values) {
  const seen = new Set();
  const duplicates = new Set();
  for (const value of values) {
    if (seen.has(value)) duplicates.add(value);
    seen.add(value);
  }
  return [...duplicates].sort();
}

function difference(left, right) {
  const rightSet = new Set(right);
  return [...new Set(left)].filter((value) => !rightSet.has(value)).sort();
}

function sameStringSet(left, right) {
  if (!Array.isArray(left) || !Array.isArray(right)) return false;
  const normalizedLeft = [...new Set(left.map(String))].sort();
  const normalizedRight = [...new Set(right.map(String))].sort();
  return (
    normalizedLeft.length === left.length &&
    normalizedRight.length === right.length &&
    normalizedLeft.length === normalizedRight.length &&
    normalizedLeft.every((value, index) => value === normalizedRight[index])
  );
}

function validateExactEvidenceShape(actual, expected, label, errors) {
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual)) {
      errors.push(`${label} must be an array`);
      return;
    }
    if (actual.length !== expected.length) {
      errors.push(`${label} must contain exactly ${expected.length} entries`);
    }
    const comparableLength = Math.min(actual.length, expected.length);
    for (let index = 0; index < comparableLength; index += 1) {
      validateExactEvidenceShape(
        actual[index],
        expected[index],
        `${label}[${index}]`,
        errors,
      );
    }
    return;
  }

  if (expected !== null && typeof expected === "object") {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual)) {
      errors.push(`${label} must be an object`);
      return;
    }
    for (const key of Object.keys(actual)) {
      if (!Object.hasOwn(expected, key)) {
        errors.push(`${label} contains unsupported field ${key}`);
      }
    }
    for (const key of Object.keys(expected)) {
      if (!Object.hasOwn(actual, key)) {
        errors.push(`${label} is missing required field ${key}`);
        continue;
      }
      validateExactEvidenceShape(actual[key], expected[key], `${label}.${key}`, errors);
    }
    return;
  }

  if (actual !== expected) {
    errors.push(`${label} must be ${JSON.stringify(expected)}`);
  }
}

function validatePendingLocalProfileEvidence(evidence, pending) {
  const errors = [];
  if (evidence === null || typeof evidence !== "object" || Array.isArray(evidence)) {
    return [
      `${pending?.gameId || "<missing-pending-game-id>"} runtime_onboarding_pending requires structured localProfileEvidence`,
    ];
  }
  const expected = {
    ...structuredClone(MAFIA3_LOCAL_PROFILE_EVIDENCE_TEMPLATE),
    expected_app_id_context: String(pending?.appId ?? ""),
    expected_steam_build_id_context: String(pending?.targetBuildId ?? ""),
    executable_basename: pending?.executableCandidate,
  };
  validateExactEvidenceShape(
    evidence,
    expected,
    `${pending?.gameId || "<missing-pending-game-id>"} localProfileEvidence`,
    errors,
  );
  return errors;
}

function finalAuthorityProvisionalReasons(record, gameIds, identityConflictGameIds) {
  const reasons = [];
  if (record?.final_for_observed_build !== true) {
    reasons.push("final_for_observed_build_false");
  }
  const evidence = Array.isArray(record?.evidence) ? record.evidence : [];
  const hasIndependentRepositoryRecord = evidence.some(
    (item) => item?.kind === "repository_record",
  );
  if (!hasIndependentRepositoryRecord) {
    const productOnly =
      evidence.length > 0 &&
      evidence.every((item) => PRODUCT_ONLY_FINAL_EVIDENCE_KINDS.has(item?.kind));
    reasons.push(
      productOnly
        ? "product_survey_or_taxonomy_only"
        : "independent_repository_record_missing",
    );
  }
  if (gameIds.some((gameId) => identityConflictGameIds.has(gameId))) {
    reasons.push("catalog_final_executable_identity_conflict");
  }
  return reasons;
}

function validateFinalLedgerAudit(
  audit,
  finalClassifications,
  rightsByAppId,
  shippedById,
  identityConflictGameIds,
) {
  const errors = [];
  if (!audit || typeof audit !== "object" || Array.isArray(audit)) {
    return ["finalLedgerAudit must be an object"];
  }
  for (const key of Object.keys(audit)) {
    if (!ALLOWED_FINAL_LEDGER_AUDIT_KEYS.has(key)) {
      errors.push(`finalLedgerAudit contains unsupported field ${key}`);
    }
  }
  if (audit.schema !== "dm-depth-final-ledger-audit-v1") {
    errors.push("finalLedgerAudit schema must be dm-depth-final-ledger-audit-v1");
  }

  const finalRecords = Array.isArray(finalClassifications?.records)
    ? finalClassifications.records
    : [];
  const expectedMappings = [];
  const expectedUnmappedLedgerAppIds = [];
  const mappedShippedGameIds = [];
  for (const record of finalRecords) {
    const appId = String(record?.app_id ?? "");
    const rightsRecord = rightsByAppId.get(appId);
    const gameIds = [
      ...new Set(
        (rightsRecord?.catalog_game_ids ?? []).filter((gameId) => shippedById.has(gameId)),
      ),
    ];
    if (gameIds.length === 0) {
      expectedUnmappedLedgerAppIds.push(appId);
      continue;
    }
    const provisionalReasons = finalAuthorityProvisionalReasons(
      record,
      gameIds,
      identityConflictGameIds,
    );
    expectedMappings.push({
      appId,
      gameIds,
      classification: record.classification,
      finalForObservedBuild: record.final_for_observed_build,
      authorityMaturity:
        provisionalReasons.length === 0
          ? "hard_terminal_for_observed_build"
          : "provisional",
      provisionalReasons,
    });
    mappedShippedGameIds.push(...gameIds);
  }

  for (const duplicate of duplicateValues(mappedShippedGameIds)) {
    errors.push(`final ledger maps shipped game more than once: ${duplicate}`);
  }
  const expectedUnmappedShippedGameIds = difference(
    [...shippedById.keys()],
    mappedShippedGameIds,
  );
  const hardMappings = expectedMappings.filter(
    (mapping) => mapping.authorityMaturity === "hard_terminal_for_observed_build",
  );
  const provisionalMappings = expectedMappings.filter(
    (mapping) => mapping.authorityMaturity === "provisional",
  );
  const hardMappedGames = hardMappings.flatMap((mapping) => mapping.gameIds);
  const provisionalMappedGames = provisionalMappings.flatMap((mapping) => mapping.gameIds);
  const expectedCounts = {
    ledgerTotalRecords: finalRecords.length,
    shippedRosterGames: shippedById.size,
    mappedLedgerRecords: expectedMappings.length,
    mappedShippedGames: new Set(mappedShippedGameIds).size,
    unmappedLedgerRecords: expectedUnmappedLedgerAppIds.length,
    unmappedShippedGames: expectedUnmappedShippedGameIds.length,
    hardTerminalMappedRecords: hardMappings.length,
    hardTerminalMappedGames: new Set(hardMappedGames).size,
    provisionalMappedRecords: provisionalMappings.length,
    provisionalMappedGames: new Set(provisionalMappedGames).size,
  };
  for (const [field, expected] of Object.entries(expectedCounts)) {
    if (audit[field] !== expected) {
      errors.push(`finalLedgerAudit ${field} must be ${expected}, got ${audit[field]}`);
    }
  }

  if (!sameStringSet(audit.unmappedLedgerAppIds, expectedUnmappedLedgerAppIds)) {
    errors.push("finalLedgerAudit unmappedLedgerAppIds does not match the full final ledger");
  }
  if (!sameStringSet(audit.unmappedShippedGameIds, expectedUnmappedShippedGameIds)) {
    errors.push("finalLedgerAudit unmappedShippedGameIds does not match the shipped roster");
  }
  if (!Array.isArray(audit.mappedRecords)) {
    errors.push("finalLedgerAudit mappedRecords must be an array");
    return errors;
  }
  for (const duplicate of duplicateValues(audit.mappedRecords.map((mapping) => mapping?.appId))) {
    errors.push(`finalLedgerAudit duplicate mapped appId: ${duplicate}`);
  }
  const declaredByAppId = new Map(
    audit.mappedRecords.map((mapping) => [String(mapping?.appId ?? ""), mapping]),
  );
  for (const expected of expectedMappings) {
    const declared = declaredByAppId.get(expected.appId);
    if (!declared) {
      errors.push(`finalLedgerAudit missing mapped final record for AppID ${expected.appId}`);
      continue;
    }
    for (const key of Object.keys(declared)) {
      if (!ALLOWED_FINAL_LEDGER_MAPPING_KEYS.has(key)) {
        errors.push(`finalLedgerAudit AppID ${expected.appId} contains unsupported field ${key}`);
      }
    }
    if (!sameStringSet(declared.gameIds, expected.gameIds)) {
      errors.push(`finalLedgerAudit AppID ${expected.appId} shipped game mapping does not match rights ledger`);
    }
    if (declared.classification !== expected.classification) {
      errors.push(`finalLedgerAudit AppID ${expected.appId} classification does not match final ledger`);
    }
    if (declared.finalForObservedBuild !== expected.finalForObservedBuild) {
      errors.push(`finalLedgerAudit AppID ${expected.appId} finalForObservedBuild does not match final ledger`);
    }
    if (!ALLOWED_FINAL_AUTHORITY_MATURITY.has(declared.authorityMaturity)) {
      errors.push(`finalLedgerAudit AppID ${expected.appId} has unknown authorityMaturity`);
    } else if (declared.authorityMaturity !== expected.authorityMaturity) {
      errors.push(
        `finalLedgerAudit AppID ${expected.appId} authorityMaturity must be ${expected.authorityMaturity}`,
      );
    }
    if (
      !Array.isArray(declared.provisionalReasons) ||
      declared.provisionalReasons.some((reason) => !ALLOWED_PROVISIONAL_REASONS.has(reason))
    ) {
      errors.push(`finalLedgerAudit AppID ${expected.appId} has invalid provisionalReasons`);
    } else if (!sameStringSet(declared.provisionalReasons, expected.provisionalReasons)) {
      errors.push(
        `finalLedgerAudit AppID ${expected.appId} provisionalReasons do not match authority evidence`,
      );
    }
  }
  for (const appId of difference([...declaredByAppId.keys()], expectedMappings.map((item) => item.appId))) {
    errors.push(`finalLedgerAudit AppID ${appId} is not mapped to the shipped roster`);
  }
  return errors;
}

function normalizeSha256(value) {
  if (typeof value !== "string") return null;
  const normalized = value.toLowerCase().startsWith("sha256:")
    ? value.slice(7).toLowerCase()
    : value.toLowerCase();
  return /^[0-9a-f]{64}$/.test(normalized) ? normalized : null;
}

function catalogExecutableIdentity(value) {
  if (value === null) return { state: "unbound", hash: null };
  const hash = normalizeSha256(value);
  return hash ? { state: "bound", hash } : { state: "malformed", hash: null };
}

function fileSha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function formalDepthIdentityErrors(
  gameId, formal, finalRecord, performanceCertifications, historicalPairs = [],
) {
  const errors = [];
  const depthExecutableHash = normalizeSha256(formal.depthProfile?.executable_sha256);
  const bindingExecutableHash = normalizeSha256(formal.cameraBinding?.executable_sha256);
  const finalExecutableHash = normalizeSha256(finalRecord?.executable?.sha256);
  const depthContentHash = normalizeSha256(formal.depthReference?.content_hash);
  const bindingContentHash = normalizeSha256(formal.bindingReference?.content_hash);

  if (!depthExecutableHash) {
    errors.push(`${gameId}: formal DepthProfile executable SHA-256 is malformed`);
  }
  if (!bindingExecutableHash) {
    errors.push(`${gameId}: formal CameraBinding executable SHA-256 is malformed`);
  }
  if (
    depthExecutableHash !== null &&
    bindingExecutableHash !== null &&
    depthExecutableHash !== bindingExecutableHash
  ) {
    errors.push(`${gameId}: formal DepthProfile and CameraBinding executable identities differ`);
  }
  if (!finalExecutableHash) {
    errors.push(`${gameId}: formal depth catalog requires an exact final-classification executable`);
  } else {
    if (depthExecutableHash && depthExecutableHash !== finalExecutableHash) {
      errors.push(`${gameId}: formal DepthProfile executable identity differs from final classification`);
    }
    if (bindingExecutableHash && bindingExecutableHash !== finalExecutableHash) {
      errors.push(`${gameId}: formal CameraBinding executable identity differs from final classification`);
    }
  }

  if (formal.depthProfile?.game_id !== gameId || formal.depthReference?.id !== gameId) {
    errors.push(`${gameId}: formal DepthProfile game identity does not match the shipped game`);
  }
  if (
    formal.cameraBinding?.profile_id !== gameId ||
    formal.cameraBinding?.binding_id !== formal.bindingReference?.id ||
    String(formal.cameraBinding?.binding_version ?? "") !==
      String(formal.bindingReference?.version ?? "") ||
    formal.cameraBinding?.profile_version !== formal.depthProfile?.schema_version
  ) {
    errors.push(`${gameId}: formal CameraBinding does not reference the exact DepthProfile identity`);
  }

  if (!depthContentHash || depthContentHash !== formal.depthContentHash) {
    errors.push(`${gameId}: formal DepthProfile manifest content hash does not match payload bytes`);
  }
  if (!bindingContentHash || bindingContentHash !== formal.bindingContentHash) {
    errors.push(`${gameId}: formal CameraBinding manifest content hash does not match payload bytes`);
  }

  const certifications = Array.isArray(performanceCertifications?.certifications)
    ? performanceCertifications.certifications.filter((entry) => entry?.key?.gameId === gameId)
    : [];
  for (const certification of certifications) {
    const certifiedDepthHash = normalizeSha256(certification?.key?.depthProfileSha256);
    const certifiedBindingHash = normalizeSha256(certification?.key?.cameraBindingSha256);
    if (
      (certifiedDepthHash !== formal.depthContentHash ||
        certifiedBindingHash !== formal.bindingContentHash) &&
      !historicalPairs.some((pair) =>
        certifiedDepthHash === pair.depthContentHash &&
        certifiedBindingHash === pair.bindingContentHash)
    ) {
      errors.push(
        `${gameId}: performance certification ${certification?.certificationId ?? "<missing-id>"} does not bind the exact formal or retained historical DepthProfile/CameraBinding bytes`,
      );
    }
  }

  return errors;
}

// Historical certificates remain immutable. They are only recognized when explicit retained
// payload bytes prove the complete old pair; these pairs never enter current applicability.
function retainedPerformancePairs(root, game, finalRecord) {
  const pairs = [];
  const errors = [];
  const declared = game?.historicalPerformancePairs ?? [];
  if (!Array.isArray(declared)) {
    return { pairs, errors: [`${game?.gameId}: historicalPerformancePairs must be an array`] };
  }
  const seen = new Set();
  for (const [index, entry] of declared.entries()) {
    const prefix = `${game.gameId}: historical performance pair ${index}`;
    if (!entry || Object.keys(entry).sort().join(",") !== "cameraBinding,depthProfile" ||
        !evidencePathExists(root, entry.depthProfile) ||
        !evidencePathExists(root, entry.cameraBinding)) {
      errors.push(`${prefix} must name safe existing depthProfile and cameraBinding evidence paths`);
      continue;
    }
    try {
      const depthProfile = loadJson(resolve(root, entry.depthProfile));
      const cameraBinding = loadJson(resolve(root, entry.cameraBinding));
      const depthContentHash = fileSha256(resolve(root, entry.depthProfile));
      const bindingContentHash = fileSha256(resolve(root, entry.cameraBinding));
      const pair = {
        depthProfile, cameraBinding, depthContentHash, bindingContentHash,
        depthReference: { id: depthProfile.game_id, content_hash: depthContentHash },
        bindingReference: {
          id: cameraBinding.binding_id, version: cameraBinding.binding_version,
          content_hash: bindingContentHash,
        },
      };
      const identityErrors = formalDepthIdentityErrors(
        game.gameId, pair, finalRecord, { certifications: [] },
      );
      if (identityErrors.length > 0) {
        errors.push(...identityErrors.map((error) => `${prefix}: ${error}`));
        continue;
      }
      const identity = `${depthContentHash}/${bindingContentHash}`;
      if (seen.has(identity)) {
        errors.push(`${prefix} duplicates retained payload bytes`);
        continue;
      }
      seen.add(identity);
      pairs.push(pair);
    } catch {
      errors.push(`${prefix} payload bytes must be readable JSON`);
    }
  }
  return { pairs, errors };
}

function hasApplicablePerformanceCertification(gameId, formal, performanceCertifications) {
  const certifications = Array.isArray(performanceCertifications?.certifications)
    ? performanceCertifications.certifications
    : [];
  const approvedRuntimes = Array.isArray(performanceCertifications?.approvedRuntimes)
    ? performanceCertifications.approvedRuntimes
    : [];
  return certifications.some((certification) => {
    const key = certification?.key;
    if (
      key?.gameId !== gameId ||
      normalizeSha256(key.depthProfileSha256) !== formal.depthContentHash ||
      normalizeSha256(key.cameraBindingSha256) !== formal.bindingContentHash
    ) {
      return false;
    }
    const generationField =
      key.rendererBackend === "d3d11"
        ? "d3d11Generation"
        : key.rendererBackend === "d3d12"
          ? "d3d12Generation"
          : null;
    return (
      generationField !== null &&
      Number.isInteger(key.backendGeneration) &&
      approvedRuntimes.some(
        (runtime) => runtime?.[generationField] === key.backendGeneration,
      )
    );
  });
}

function selfTestHistoricalPerformancePairs(formal, finalRecord) {
  const root = mkdtempSync(join(realpathSync(tmpdir()), "dm-historical-performance-pair-"));
  const gameId = formal.depthProfile.game_id;
  const oldBinding = structuredClone(formal.cameraBinding);
  oldBinding.binding_id += "-historical-fixture";
  oldBinding.binding_version = "historical-fixture";
  const game = {
    gameId,
    historicalPerformancePairs: [{ depthProfile: "depth.json", cameraBinding: "binding.json" }],
  };
  try {
    writeFileSync(join(root, "depth.json"), `${JSON.stringify(formal.depthProfile)}\n`);
    writeFileSync(join(root, "binding.json"), `${JSON.stringify(oldBinding)}\n`);
    const historical = retainedPerformancePairs(root, game, finalRecord);
    if (historical.errors.length || historical.pairs.length !== 1) {
      throw new Error(`self-test rejected retained historical bytes: ${historical.errors.join("; ")}`);
    }
    const manifest = {
      approvedRuntimes: [{ d3d11Generation: 4, d3d12Generation: 9 }],
      certifications: [{
        certificationId: "self-test-retained-historical-pair",
        key: {
          gameId, rendererBackend: "d3d11", backendGeneration: 4,
          depthProfileSha256: historical.pairs[0].depthContentHash,
          cameraBindingSha256: historical.pairs[0].bindingContentHash,
        },
      }],
    };
    if (formalDepthIdentityErrors(gameId, formal, finalRecord, manifest, historical.pairs).length) {
      throw new Error("self-test rejected a certificate bound to retained historical bytes");
    }
    if (hasApplicablePerformanceCertification(gameId, formal, manifest)) {
      throw new Error("self-test historical certification incorrectly authorizes the current pair");
    }
    if (!formalDepthIdentityErrors(gameId, formal, finalRecord, manifest).length) {
      throw new Error("self-test accepted historical certificate hashes without retained evidence");
    }
    for (const [name, mutate] of [
      ["unknown hash", (key) => { key.cameraBindingSha256 = "b".repeat(64); }],
      ["mixed old/current pair", (key) => { key.cameraBindingSha256 = formal.bindingContentHash; }],
    ]) {
      const bad = structuredClone(manifest);
      mutate(bad.certifications[0].key);
      if (!formalDepthIdentityErrors(gameId, formal, finalRecord, bad, historical.pairs).length) {
        throw new Error(`self-test accepted ${name} as historical certification evidence`);
      }
    }
    const badGame = { ...game, gameId: "wrong-history-game" };
    if (!retainedPerformancePairs(root, badGame, finalRecord).errors.length) {
      throw new Error("self-test accepted historical evidence for another game");
    }
    const unsafe = structuredClone(game);
    unsafe.historicalPerformancePairs[0].cameraBinding = "../binding.json";
    if (!retainedPerformancePairs(root, unsafe, finalRecord).errors.length) {
      throw new Error("self-test accepted an unsafe historical evidence path");
    }
    writeFileSync(join(root, "binding.json"), `${JSON.stringify(oldBinding)} \n`);
    const changedBytes = retainedPerformancePairs(root, game, finalRecord);
    if (changedBytes.errors.length ||
        !formalDepthIdentityErrors(gameId, formal, finalRecord, manifest, changedBytes.pairs).length) {
      throw new Error("self-test changed historical bytes retained the old certificate hash");
    }
    oldBinding.executable_sha256 = `sha256:${"b".repeat(64)}`;
    writeFileSync(join(root, "binding.json"), JSON.stringify(oldBinding));
    if (!retainedPerformancePairs(root, game, finalRecord).errors.length) {
      throw new Error("self-test accepted a historical binding/profile executable mismatch");
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function hasExactRuntimeGateEvidence(root, finalRecord) {
  for (const evidence of finalRecord?.evidence ?? []) {
    if (evidence?.kind !== "repository_record" || typeof evidence.repo_path !== "string") {
      continue;
    }
    const normalizedRepoPath = evidence.repo_path.replaceAll("\\", "/");
    const relativeEvidencePath = normalizedRepoPath.startsWith("DataMiner/")
      ? normalizedRepoPath.slice("DataMiner/".length)
      : normalizedRepoPath;
    if (!evidencePathExists(root, relativeEvidencePath)) continue;
    try {
      if (
        [
          "game-runtime-gate-evidence/v1",
          "game-runtime-gate-evidence-redacted/v1",
        ].includes(loadJson(resolve(root, relativeEvidencePath))?.schema)
      ) {
        return true;
      }
    } catch {
      // A malformed record is not exact runtime evidence; the caller fails closed below.
    }
  }
  return false;
}

function safeWindowsRelativeBasename(value) {
  if (typeof value !== "string" || value.length === 0 || value.trim() !== value) return null;
  const segments = value.split(/[\\/]/);
  if (
    win32.isAbsolute(value) ||
    segments.some(
      (segment) =>
        segment.length === 0 || segment === "." || segment === ".." || segment.includes(":"),
    )
  ) {
    return null;
  }
  return segments.at(-1).toLowerCase();
}

function normalizedWindowsAbsoluteExecutableBasename(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.trim() !== value ||
    value.includes("/") ||
    !/^[A-Za-z]:\\/.test(value) ||
    !win32.isAbsolute(value) ||
    win32.normalize(value) !== value
  ) {
    return null;
  }
  const basename = win32.basename(value);
  if (
    basename.length === 0 ||
    basename === "." ||
    basename === ".." ||
    !basename.toLowerCase().endsWith(".exe")
  ) {
    return null;
  }
  return basename.toLowerCase();
}

function safeWindowsExecutableBasename(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.trim() !== value ||
    value.includes("/") ||
    value.includes("\\") ||
    value.includes(":") ||
    !value.toLowerCase().endsWith(".exe")
  ) {
    return null;
  }
  return value.toLowerCase();
}

function mergeRendererIdentityAuthority(current, candidate, source, errors, gameId) {
  const merged = { ...current };
  for (const field of ["appId", "buildId", "executableBasename", "executableSha256"]) {
    const value = candidate?.[field];
    if (value === null || value === undefined) continue;
    const normalized =
      field === "executableBasename"
        ? String(value).toLowerCase()
        : field === "executableSha256"
          ? normalizeSha256(value)
          : String(value);
    if (normalized === null) continue;
    if (merged[field] !== undefined && merged[field] !== normalized) {
      errors.push(
        `${gameId}: renderer executable authority conflicts between ${merged[`${field}Source`]} and ${source}`,
      );
      continue;
    }
    merged[field] = normalized;
    merged[`${field}Source`] = source;
  }
  return merged;
}

function localTrialIdentityAuthority(root, game) {
  if (game?.rendererEvidence?.apiEvidenceTier !== "exact_build_local_trial") return null;
  for (const evidencePath of game?.evidence ?? []) {
    if (!evidencePathExists(root, evidencePath) || !evidencePath.toLowerCase().endsWith(".json")) {
      continue;
    }
    try {
      const record = loadJson(resolve(root, evidencePath));
      if (
        record?.schema === "dm-cp2077-metric-promotion-evidence/v1" &&
        record?.game?.game_id === game.gameId
      ) {
        return {
          appId: String(record.game.steam_app_id ?? ""),
          buildId: String(record.game.steam_build_id ?? ""),
          executableSha256: record.game.executable_sha256,
          source: evidencePath,
        };
      }
    } catch {
      // Unsupported or malformed JSON cannot become an executable-identity authority.
    }
  }
  return null;
}

function validateExactInstallIdentityRecord(record, gameId, source) {
  const errors = [];
  const prefix = `${gameId}: ${source}`;
  const fail = (message) => errors.push(`${prefix} ${message}`);
  if (record?.schema_version !== "dm-steam-exact-install-evidence-1") {
    fail("must use dm-steam-exact-install-evidence-1");
    return { authority: null, errors };
  }

  const recordGameId = record?.game?.game_id;
  const appId = String(record?.game?.steam_app_id ?? "");
  const install = record?.steam_install_gate;
  const executable = record?.shipping_executable;
  const executableBasename = safeWindowsRelativeBasename(executable?.relative_path);
  const executableSha256 = normalizeSha256(executable?.sha256);
  const buildId = String(install?.build_id ?? "");
  const manifest = install?.manifest_snapshot;
  const manifestSha256 = normalizeSha256(manifest?.sha256);
  if (recordGameId !== gameId) fail(`game.game_id must be ${gameId}`);
  if (!/^\d+$/.test(appId)) fail("game.steam_app_id must be a decimal AppID");
  if (install?.status !== "install_gate_passed") {
    fail("steam_install_gate.status must be install_gate_passed");
  }
  if (install?.state_flags !== 4) fail("steam_install_gate.state_flags must be 4");
  if (!/^\d+$/.test(buildId)) fail("steam_install_gate.build_id must be a decimal build ID");
  if (String(install?.target_build_id ?? "") !== buildId) {
    fail("steam_install_gate.target_build_id must equal build_id");
  }
  if (install?.staging_size_bytes !== 0 || install?.update_result !== 0) {
    fail("steam_install_gate must prove zero staging bytes and update_result=0");
  }
  if (
    !Number.isInteger(install?.stability_observations) ||
    install.stability_observations < 2
  ) {
    fail("steam_install_gate requires at least two stability observations");
  }
  if (install?.install_path_chain_reparse_free !== true) {
    fail("steam_install_gate must prove a reparse-free install path chain");
  }
  if (manifest?.relative_path !== `steamapps/appmanifest_${appId}.acf`) {
    fail("steam_install_gate manifest relative_path must canonically match the evidence AppID");
  }
  if (!Number.isSafeInteger(manifest?.length_bytes) || manifest.length_bytes <= 0) {
    fail("steam_install_gate manifest length_bytes must be a positive safe integer");
  }
  if (!manifestSha256 || manifest.sha256 !== `sha256:${manifestSha256}`) {
    fail("steam_install_gate manifest sha256 must use canonical sha256:<lowercase-hex> form");
  }
  if (manifest?.reparse_point !== false) {
    fail("steam_install_gate manifest snapshot must not be a reparse point");
  }
  if (
    !Number.isSafeInteger(install?.stability_interval_seconds) ||
    install.stability_interval_seconds <= 0
  ) {
    fail("steam_install_gate stability_interval_seconds must be a positive safe integer");
  }
  if (!executableBasename) {
    fail("shipping_executable.relative_path must be a safe relative executable path");
  }
  if (!executableSha256) fail("shipping_executable.sha256 must be a valid SHA-256");
  if (executable?.reparse_point !== false || executable?.stable_across_observations !== true) {
    fail("shipping_executable must be stable and must not be a reparse point");
  }

  const safety = record?.safety_boundary;
  if (
    safety?.observation_mode !== "read_only_installed_bytes" ||
    safety?.game_launched_for_this_observation !== false ||
    safety?.staging_or_downloading_bytes_read !== false ||
    safety?.absolute_paths_recorded !== false ||
    safety?.account_device_or_process_identifiers_recorded !== false ||
    safety?.online_or_protection_bypass_authorized !== false
  ) {
    fail("safety_boundary must remain read-only, sanitized, and no-bypass");
  }

  const runtime = record?.runtime_admission_state;
  if (
    runtime?.d6_complete !== false ||
    runtime?.d7_passed !== false ||
    runtime?.depth_capture_available !== false ||
    runtime?.recording_ready !== false
  ) {
    fail("exact-install evidence must not authorize D6, D7, Depth capture, or recording");
  }
  if (runtime?.install_tree_pristine !== "not_asserted") {
    fail("runtime_admission_state.install_tree_pristine must remain not_asserted");
  }
  const rights = record?.rights_gate_observation;
  if (String(rights?.app_id ?? "") !== appId) {
    fail("rights_gate_observation.app_id must equal game.steam_app_id");
  }
  if (rights?.runtime_anti_cheat_gate_still_required !== true) {
    fail("rights_gate_observation must keep the runtime anti-cheat gate required");
  }
  if (
    record?.catalog_update?.window_class !== null ||
    record?.catalog_update?.depth_profile_present !== false ||
    record?.catalog_update?.camera_binding_present !== false
  ) {
    fail("exact-install evidence must not claim a window, DepthProfile, or CameraBinding");
  }

  return {
    authority:
      errors.length === 0
        ? {
            appId,
            buildId,
            executableBasename,
            executableSha256,
            source,
            requiresCleanInstallSurface: true,
          }
        : null,
    errors,
  };
}

function exactInstallIdentityAuthority(root, game) {
  const results = [];
  const errors = [];
  for (const evidencePath of game?.evidence ?? []) {
    if (!evidencePathExists(root, evidencePath) || !evidencePath.toLowerCase().endsWith(".json")) {
      continue;
    }
    try {
      const record = loadJson(resolve(root, evidencePath));
      if (record?.schema_version !== "dm-steam-exact-install-evidence-1") continue;
      const validated = validateExactInstallIdentityRecord(record, game.gameId, evidencePath);
      errors.push(...validated.errors);
      if (validated.authority) results.push(validated.authority);
    } catch {
      errors.push(`${game.gameId}: exact-install evidence is not valid JSON: ${evidencePath}`);
    }
  }
  if (results.length > 1) {
    errors.push(`${game.gameId}: multiple exact-install identity authorities are not supported`);
  }
  return { authority: results.length === 1 ? results[0] : null, errors };
}

function validateRendererEvidence(root, game, identityAuthority = {}) {
  const prefix = game?.gameId || "<missing-game-id>";
  const errors = [];
  const renderer = game?.rendererEvidence;
  if (renderer === null || typeof renderer !== "object" || Array.isArray(renderer)) {
    return [`${prefix} rendererEvidence must be an object`];
  }
  for (const key of Object.keys(renderer)) {
    if (!ALLOWED_RENDERER_EVIDENCE_KEYS.has(key)) {
      errors.push(`${prefix} rendererEvidence contains unsupported field ${key}`);
    }
  }
  if (renderer.schema !== "dm-renderer-backend-evidence-v2") {
    errors.push(`${prefix} rendererEvidence schema is unsupported`);
  }

  const graphicsApis = renderer.graphicsApis;
  if (!Array.isArray(graphicsApis) || graphicsApis.length === 0) {
    errors.push(`${prefix} rendererEvidence graphicsApis must be a non-empty array`);
  } else {
    for (const graphicsApi of graphicsApis) {
      if (!ALLOWED_RENDERER_APIS.has(graphicsApi)) {
        errors.push(`${prefix} rendererEvidence has unsupported graphics API ${graphicsApi}`);
      }
    }
    if (new Set(graphicsApis).size !== graphicsApis.length) {
      errors.push(`${prefix} rendererEvidence graphicsApis must not contain duplicates`);
    }
    const canonicalApis = [...graphicsApis].sort(
      (left, right) => RENDERER_API_ORDER.indexOf(left) - RENDERER_API_ORDER.indexOf(right),
    );
    if (JSON.stringify(graphicsApis) !== JSON.stringify(canonicalApis)) {
      errors.push(`${prefix} rendererEvidence graphicsApis must use canonical order`);
    }
    if (graphicsApis.includes("unknown") && graphicsApis.length !== 1) {
      errors.push(`${prefix} rendererEvidence unknown graphics API must be exclusive`);
    }
  }

  if (!ALLOWED_RENDERER_MODES.has(renderer.rendererMode)) {
    errors.push(`${prefix} rendererEvidence has unsupported rendererMode ${renderer.rendererMode}`);
  }
  const expectedMode =
    Array.isArray(graphicsApis) && graphicsApis.length === 1 && graphicsApis[0] === "unknown"
      ? "unknown"
      : Array.isArray(graphicsApis) && graphicsApis.length === 1
        ? "single"
        : Array.isArray(graphicsApis) && graphicsApis.length > 1
          ? "multi"
          : null;
  if (expectedMode !== null && renderer.rendererMode !== expectedMode) {
    errors.push(
      `${prefix} rendererEvidence rendererMode ${renderer.rendererMode} does not match graphicsApis (${expectedMode})`,
    );
  }
  if (!ALLOWED_API_EVIDENCE_TIERS.has(renderer.apiEvidenceTier)) {
    errors.push(
      `${prefix} rendererEvidence has unsupported apiEvidenceTier ${renderer.apiEvidenceTier}`,
    );
  }

  const isUnknown = expectedMode === "unknown";
  if (isUnknown) {
    if (renderer.apiEvidenceTier !== "unknown") {
      errors.push(`${prefix} unknown renderer must use unknown apiEvidenceTier`);
    }
    if (renderer.evidence !== null) {
      errors.push(`${prefix} unknown renderer must keep renderer evidence null`);
    }
    if (Object.hasOwn(renderer, "exactBuildIdentity")) {
      errors.push(`${prefix} unknown renderer must not declare an exact-build identity`);
    }
    return errors;
  }
  if (renderer.apiEvidenceTier === "unknown") {
    errors.push(`${prefix} known renderer API cannot use unknown apiEvidenceTier`);
  }

  const evidence = renderer.evidence;
  if (evidence === null || typeof evidence !== "object" || Array.isArray(evidence)) {
    errors.push(`${prefix} known renderer API requires a structured evidence locator`);
    return errors;
  }
  const allowedEvidenceKeys = new Set(["path", "locator"]);
  for (const key of Object.keys(evidence)) {
    if (!allowedEvidenceKeys.has(key)) {
      errors.push(`${prefix} renderer evidence locator contains unsupported field ${key}`);
    }
  }
  if (!evidencePathExists(root, evidence.path)) {
    errors.push(`${prefix} renderer evidence path is missing or unsafe: ${evidence.path}`);
  }
  if (typeof evidence.locator !== "string" || evidence.locator.trim().length === 0) {
    errors.push(`${prefix} renderer evidence locator must be a non-empty string`);
  }
  if (!Array.isArray(game.evidence) || !game.evidence.includes(evidence.path)) {
    errors.push(`${prefix} renderer evidence path must also appear in the game evidence list`);
  }

  const isExactBuild = EXACT_BUILD_API_EVIDENCE_TIERS.has(renderer.apiEvidenceTier);
  if (!isExactBuild && Object.hasOwn(renderer, "exactBuildIdentity")) {
    errors.push(`${prefix} non-exact renderer evidence must not declare an exact-build identity`);
  }

  let exactIdentity = null;
  if (isExactBuild) {
    const candidate = renderer.exactBuildIdentity;
    if (candidate === null || typeof candidate !== "object" || Array.isArray(candidate)) {
      errors.push(`${prefix} exact renderer evidence requires exactBuildIdentity`);
    } else {
      exactIdentity = candidate;
      for (const key of Object.keys(candidate)) {
        if (!ALLOWED_EXACT_BUILD_IDENTITY_KEYS.has(key)) {
          errors.push(`${prefix} exactBuildIdentity contains unsupported field ${key}`);
        }
      }
      for (const key of ALLOWED_EXACT_BUILD_IDENTITY_KEYS) {
        if (!Object.hasOwn(candidate, key)) {
          errors.push(`${prefix} exactBuildIdentity is missing ${key}`);
        }
      }
      if (!/^\d+$/.test(candidate.appId ?? "")) {
        errors.push(`${prefix} exactBuildIdentity appId must be a decimal string`);
      }
      if (candidate.buildId !== null && !/^\d+$/.test(candidate.buildId ?? "")) {
        errors.push(`${prefix} exactBuildIdentity buildId must be null or a decimal string`);
      }
      if (!safeWindowsExecutableBasename(candidate.executableBasename)) {
        errors.push(`${prefix} exactBuildIdentity executableBasename must be one safe EXE basename`);
      }
      if (!/^sha256:[0-9a-f]{64}$/.test(candidate.executableSha256 ?? "")) {
        errors.push(`${prefix} exactBuildIdentity executableSha256 must use lowercase sha256 form`);
      }

      const normalizedIdentityHash = normalizeSha256(candidate.executableSha256);
      const normalizedIdentityBasename = safeWindowsExecutableBasename(
        candidate.executableBasename,
      );
      for (const [field, actual, expected] of [
        ["appId", String(candidate.appId ?? ""), identityAuthority.appId],
        ["executableBasename", normalizedIdentityBasename, identityAuthority.executableBasename],
        ["executableSha256", normalizedIdentityHash, identityAuthority.executableSha256],
      ]) {
        if (expected === undefined || expected === null) {
          errors.push(`${prefix} exactBuildIdentity ${field} has no independent authority`);
        } else if (actual !== expected) {
          errors.push(
            `${prefix} exactBuildIdentity ${field} differs from ${identityAuthority[`${field}Source`] ?? "independent authority"}`,
          );
        }
      }

      const trialAuthority = localTrialIdentityAuthority(root, game);
      const authoritativeBuildId = identityAuthority.buildId ?? trialAuthority?.buildId ?? null;
      if (candidate.buildId !== authoritativeBuildId) {
        errors.push(
          `${prefix} exactBuildIdentity buildId differs from ${identityAuthority.buildIdSource ?? trialAuthority?.source ?? "available authority"}`,
        );
      }
      if (trialAuthority) {
        if (String(candidate.appId ?? "") !== trialAuthority.appId) {
          errors.push(`${prefix} local-trial AppID differs from structured promotion evidence`);
        }
        if (normalizedIdentityHash !== normalizeSha256(trialAuthority.executableSha256)) {
          errors.push(
            `${prefix} local-trial executable SHA-256 differs from structured promotion evidence`,
          );
        }
      }

      if (typeof evidence.locator === "string") {
        const normalizedLocator = evidence.locator.toLowerCase();
        const locatorSubjectTokens = [
          String(candidate.appId ?? "").toLowerCase(),
          String(candidate.executableBasename ?? "").toLowerCase(),
          String(game.gameId ?? "").toLowerCase(),
        ].filter((token) => token.length > 0);
        if (!locatorSubjectTokens.some((token) => normalizedLocator.includes(token))) {
          errors.push(`${prefix} exact renderer locator must name its AppID, gameId, or executable`);
        }
        for (const graphicsApi of graphicsApis ?? []) {
          if (!normalizedLocator.includes(String(graphicsApi).toLowerCase())) {
            errors.push(
              `${prefix} exact renderer locator must name declared graphics API ${graphicsApi}`,
            );
          }
        }
      }

      if (evidencePathExists(root, evidence.path)) {
        try {
          const evidenceText = readFileSync(resolve(root, evidence.path), "utf8").toLowerCase();
          const identityHashToken = normalizedIdentityHash ?? "";
          const evidenceSubjectTokens = [
            String(candidate.appId ?? "").toLowerCase(),
            String(candidate.executableBasename ?? "").toLowerCase(),
            String(game.gameId ?? "").toLowerCase(),
          ].filter((token) => token.length > 0);
          if (!identityHashToken || !evidenceText.includes(identityHashToken)) {
            errors.push(`${prefix} exact renderer evidence bytes do not contain the bound executable hash`);
          }
          if (!evidenceSubjectTokens.some((token) => evidenceText.includes(token))) {
            errors.push(`${prefix} exact renderer evidence bytes do not contain a bound subject token`);
          }
        } catch {
          errors.push(`${prefix} exact renderer evidence bytes are unreadable`);
        }
      }
    }
  }

  if (
    isExactBuild &&
    ["product_survey_only", "product_policy_survey", "catalog_only"].includes(game.evidenceTier)
  ) {
    errors.push(
      `${prefix} ${renderer.apiEvidenceTier} renderer evidence conflicts with ${game.evidenceTier} capability evidence`,
    );
  }
  if (renderer.apiEvidenceTier === "exact_build_local_trial") {
    if (game.capability !== "local_trial" || renderer.rendererMode !== "single") {
      errors.push(
        `${prefix} exact_build_local_trial renderer evidence requires one local_trial API`,
      );
    }
  }
  if (
    ["catalog_ready", "local_trial"].includes(game.capability) &&
    !["exact_build_runtime_report", "exact_build_local_trial"].includes(
      renderer.apiEvidenceTier,
    )
  ) {
    errors.push(
      `${prefix} enabled Depth capability requires exact-build runtime renderer evidence`,
    );
  }
  if (renderer.apiEvidenceTier === "exact_build_runtime_report") {
    if (renderer.rendererMode !== "single") {
      errors.push(`${prefix} exact runtime report must identify one graphics API`);
    }
    if (evidencePathExists(root, evidence.path)) {
      try {
        const report = loadJson(resolve(root, evidence.path));
        const rawRuntimeReport = report?.schema === "game-runtime-gate-evidence/v1";
        const redactedRuntimeReport =
          report?.schema === "game-runtime-gate-evidence-redacted/v1";
        if (!rawRuntimeReport && !redactedRuntimeReport) {
          errors.push(`${prefix} exact runtime renderer evidence has the wrong report schema`);
        }
        if (report?.status !== "pass") {
          errors.push(`${prefix} exact runtime renderer evidence must have pass status`);
        }
        if (report?.graphics?.expected_api !== graphicsApis?.[0]) {
          errors.push(
            `${prefix} renderer API does not match exact runtime report graphics.expected_api`,
          );
        }
        const expectedModule = {
          d3d11: "d3d11.dll",
          d3d12: "d3d12.dll",
          vulkan: "vulkan-1.dll",
        }[graphicsApis?.[0]];
        const moduleHits = Array.isArray(report?.graphics?.hits)
          ? report.graphics.hits.map((moduleName) => String(moduleName).toLowerCase())
          : [];
        if (!expectedModule || !moduleHits.includes(expectedModule)) {
          errors.push(`${prefix} exact runtime renderer evidence lacks the expected API module`);
        }
        const finalBasename = identityAuthority.executableBasename;
        const finalHash = identityAuthority.executableSha256;
        const reportBasename = rawRuntimeReport
          ? normalizedWindowsAbsoluteExecutableBasename(report?.process?.executable_path)
          : safeWindowsExecutableBasename(report?.process?.executable_basename);
        if (!reportBasename) {
          errors.push(
            rawRuntimeReport
              ? `${prefix} exact runtime renderer report must use a normalized absolute Windows executable path`
              : `${prefix} redacted runtime renderer report must retain one safe executable basename`,
          );
        }
        const reportHashValue = report?.process?.executable_sha256;
        const reportHash = /^sha256:[0-9a-f]{64}$/.test(reportHashValue ?? "")
          ? reportHashValue.slice(7)
          : null;
        if (!reportHash) {
          errors.push(
            `${prefix} exact runtime renderer report executable SHA-256 is malformed`,
          );
        }
        if (redactedRuntimeReport) {
          if (!/^sha256:[0-9a-f]{64}$/.test(report?.source_report_sha256 ?? "")) {
            errors.push(`${prefix} redacted runtime renderer report must pin the source report SHA-256`);
          }
          if (!Number.isSafeInteger(report?.source_report_size_bytes) || report.source_report_size_bytes <= 0) {
            errors.push(`${prefix} redacted runtime renderer report must pin a positive source byte size`);
          }
          const requiredRedactions = [
            "host",
            "process_id",
            "executable_path",
            "process_start_time_utc",
            "module_inventory_non_graphics",
          ];
          if (
            !Array.isArray(report?.redactions) ||
            report.redactions.length !== requiredRedactions.length ||
            requiredRedactions.some((value, index) => report.redactions[index] !== value)
          ) {
            errors.push(`${prefix} redacted runtime renderer report must declare the canonical redactions`);
          }
          if (
            report?.module_enumeration?.complete !== true ||
            report?.anti_cheat?.require_none_loaded !== true ||
            report?.anti_cheat?.service_enumeration_complete !== true ||
            report?.anti_cheat?.driver_enumeration_complete !== true ||
            report?.anti_cheat?.running_service_hits !== 0 ||
            report?.anti_cheat?.running_driver_hits !== 0 ||
            !Array.isArray(report?.anti_cheat?.module_hits) ||
            report.anti_cheat.module_hits.length !== 0
          ) {
            errors.push(`${prefix} redacted runtime renderer report does not preserve the complete clean runtime gates`);
          }
        }
        if (
          finalBasename &&
          finalHash &&
          reportBasename &&
          reportHash &&
          (reportBasename !== finalBasename || reportHash !== finalHash)
        ) {
          errors.push(
            `${prefix} exact runtime renderer report process identity differs from independent executable authority`,
          );
        }
      } catch {
        errors.push(`${prefix} exact runtime renderer evidence is not valid JSON`);
      }
    }
  }
  return errors;
}

function selfTestRendererEvidenceFixtures() {
  const canonicalTemp = realpathSync(tmpdir());
  const fixtureRoot = mkdtempSync(join(canonicalTemp, "dm-renderer-evidence-fixture-"));
  const evidenceRoot = join(fixtureRoot, "evidence");
  const alphaHash = "a".repeat(64);
  const betaHash = "b".repeat(64);
  const alphaAuthority = {
    appId: "100",
    appIdSource: "synthetic rights ledger",
    executableBasename: "alpha.exe",
    executableBasenameSource: "synthetic final classification",
    executableSha256: alphaHash,
    executableSha256Source: "synthetic final classification",
  };
  const alphaGame = {
    gameId: "synthetic-alpha",
    evidenceTier: "exact_build_runtime",
    capability: "adaptation_blocked",
    evidence: ["evidence/alpha-runtime.json"],
    rendererEvidence: {
      schema: "dm-renderer-backend-evidence-v2",
      graphicsApis: ["d3d11"],
      rendererMode: "single",
      apiEvidenceTier: "exact_build_runtime_report",
      exactBuildIdentity: {
        appId: "100",
        buildId: null,
        executableBasename: "Alpha.exe",
        executableSha256: `sha256:${alphaHash}`,
      },
      evidence: {
        path: "evidence/alpha-runtime.json",
        locator: "Alpha.exe / d3d11 process identity + graphics.expected_api",
      },
    },
  };
  try {
    mkdirSync(evidenceRoot);
    for (const [name, executableBasename, executableHash] of [
      ["alpha", "Alpha.exe", alphaHash],
      ["beta", "Beta.exe", betaHash],
    ]) {
      writeFileSync(
        join(evidenceRoot, `${name}-runtime.json`),
        `${JSON.stringify(
          {
            schema: "game-runtime-gate-evidence/v1",
            status: "pass",
            process: {
              executable_path: `C:\\Games\\${executableBasename}`,
              executable_sha256: `sha256:${executableHash}`,
            },
            graphics: { expected_api: "d3d11", hits: ["d3d11.dll", "dxgi.dll"] },
          },
          null,
          2,
        )}\n`,
      );
    }
    writeFileSync(
      join(evidenceRoot, "alpha-observation.md"),
      `AppID 100 | Alpha.exe | ${alphaHash} | D3D11\n`,
    );
    writeFileSync(
      join(evidenceRoot, "hades-observation.md"),
      `AppID 200 | Hades2.exe | ${betaHash} | D3D12\n`,
    );

    const baselineErrors = validateRendererEvidence(
      fixtureRoot,
      alphaGame,
      alphaAuthority,
    );
    if (baselineErrors.length > 0) {
      throw new Error(
        `self-test rejected synthetic exact runtime renderer evidence: ${baselineErrors.join("; ")}`,
      );
    }

    const swappedRuntime = structuredClone(alphaGame);
    swappedRuntime.rendererEvidence.evidence = {
      path: "evidence/beta-runtime.json",
      locator: "Beta.exe / process identity + graphics.expected_api",
    };
    swappedRuntime.evidence.push("evidence/beta-runtime.json");
    const swappedRuntimeErrors = validateRendererEvidence(
      fixtureRoot,
      swappedRuntime,
      alphaAuthority,
    );
    if (
      !swappedRuntimeErrors.some((error) =>
        error.includes("exact runtime renderer report process identity differs"),
      )
    ) {
      throw new Error(
        `self-test accepted a cross-game same-API synthetic runtime report: ${swappedRuntimeErrors.join("; ")}`,
      );
    }

    const observedGame = structuredClone(alphaGame);
    observedGame.rendererEvidence.apiEvidenceTier = "exact_build_observation";
    observedGame.rendererEvidence.evidence = {
      path: "evidence/alpha-observation.md",
      locator: "AppID 100 / Alpha.exe D3D11 observation",
    };
    observedGame.evidence = ["evidence/alpha-observation.md"];
    const observedErrors = validateRendererEvidence(
      fixtureRoot,
      observedGame,
      alphaAuthority,
    );
    if (observedErrors.length > 0) {
      throw new Error(
        `self-test rejected synthetic exact renderer observation: ${observedErrors.join("; ")}`,
      );
    }
    const mismatchedObservedApi = structuredClone(observedGame);
    mismatchedObservedApi.rendererEvidence.graphicsApis = ["d3d12"];
    const mismatchedObservedApiErrors = validateRendererEvidence(
      fixtureRoot,
      mismatchedObservedApi,
      alphaAuthority,
    );
    if (
      !mismatchedObservedApiErrors.some((error) =>
        error.includes("exact renderer locator must name declared graphics API d3d12"),
      )
    ) {
      throw new Error(
        `self-test accepted an exact observation with a detached graphics API: ${mismatchedObservedApiErrors.join("; ")}`,
      );
    }
    observedGame.rendererEvidence.evidence = {
      path: "evidence/hades-observation.md",
      locator: "AppID 200 / Hades2.exe D3D12 observation",
    };
    observedGame.evidence.push("evidence/hades-observation.md");
    const swappedObservationErrors = validateRendererEvidence(
      fixtureRoot,
      observedGame,
      alphaAuthority,
    );
    if (
      !swappedObservationErrors.some((error) =>
        error.includes("exact renderer evidence bytes do not contain the bound executable hash"),
      )
    ) {
      throw new Error(
        `self-test accepted a cross-game synthetic renderer observation: ${swappedObservationErrors.join("; ")}`,
      );
    }
  } finally {
    const resolved = resolve(fixtureRoot);
    const relativePath = relative(canonicalTemp, resolved);
    if (
      relativePath.length > 0 &&
      relativePath !== ".." &&
      !relativePath.startsWith(`..${sep}`)
    ) {
      rmSync(resolved, { recursive: true, force: true });
    }
  }
}

export function validateDepthCapabilities(
  capabilities,
  shipped,
  finalClassifications,
  rightsGates,
  performanceCertifications = loadJson(PERFORMANCE_CERTIFICATIONS_PATH),
  root = ROOT,
) {
  const errors = [];
  for (const key of Object.keys(capabilities ?? {})) {
    if (!ALLOWED_TOP_LEVEL_KEYS.has(key)) {
      errors.push(`capability audit contains unsupported top-level field ${key}`);
    }
  }
  if (capabilities?.schemaVersion !== "dm-game-depth-capabilities-3") {
    errors.push("schemaVersion must be dm-game-depth-capabilities-3");
  }
  if (!/^\d{4}-\d{2}-\d{2}$/.test(capabilities?.reviewedAt ?? "")) {
    errors.push("reviewedAt must use YYYY-MM-DD");
  }
  if (!Array.isArray(capabilities?.games)) {
    return [...errors, "games must be an array"];
  }
  if (!Array.isArray(shipped?.games)) {
    return [...errors, "shipped-games.json games must be an array"];
  }

  const shippedIds = shipped.games.map((game) => game.gameId);
  const capabilityIds = capabilities.games.map((game) => game.gameId);
  for (const duplicate of duplicateValues(capabilityIds)) {
    errors.push(`duplicate depth capability gameId: ${duplicate}`);
  }
  for (const gameId of difference(shippedIds, capabilityIds)) {
    errors.push(`shipped game has no depth capability state: ${gameId}`);
  }
  for (const gameId of difference(capabilityIds, shippedIds)) {
    errors.push(`depth capability game is not shipped: ${gameId}`);
  }

  const shippedById = new Map(shipped.games.map((game) => [game.gameId, game]));
  const capabilityById = new Map(capabilities.games.map((game) => [game.gameId, game]));
  const catalogsWithFormalDepth = new Set();
  const catalogGameProfileByGameId = new Map();
  const formalDepthByGameId = new Map();
  for (const shippedGame of shipped.games) {
    const catalogPath = join(
      root,
      "config",
      "catalogs",
      "dev",
      shippedGame.catalogDir,
      "catalog.json",
    );
    if (!existsSync(catalogPath)) {
      errors.push(`shipped catalog is missing: ${shippedGame.catalogDir}`);
      continue;
    }
    const catalog = loadJson(catalogPath);
    const gameProfilePath = join(
      root,
      "config",
      "catalogs",
      "dev",
      shippedGame.catalogDir,
      "game-profile.json",
    );
    if (!existsSync(gameProfilePath)) {
      errors.push(`shipped game profile is missing: ${shippedGame.catalogDir}`);
    } else {
      const gameProfile = loadJson(gameProfilePath);
      catalogGameProfileByGameId.set(shippedGame.gameId, gameProfile);
    }
    const depthCount = Array.isArray(catalog.depth_profiles) ? catalog.depth_profiles.length : 0;
    const bindingCount = Array.isArray(catalog.camera_bindings) ? catalog.camera_bindings.length : 0;
    if ((depthCount === 0) !== (bindingCount === 0)) {
      errors.push(`${shippedGame.gameId} catalog must carry DepthProfile and CameraBinding together`);
    }
    if (depthCount > 0 || bindingCount > 0) {
      if (depthCount !== 1 || bindingCount !== 1) {
        errors.push(`${shippedGame.gameId} formal depth catalog must carry exactly one profile and binding`);
      }
      catalogsWithFormalDepth.add(shippedGame.gameId);
      const depthPath = join(root, "config", "catalogs", "dev", shippedGame.catalogDir, "depth-profile.json");
      const bindingPath = join(root, "config", "catalogs", "dev", shippedGame.catalogDir, "camera-binding.json");
      if (!existsSync(depthPath) || !existsSync(bindingPath)) {
        errors.push(`${shippedGame.gameId} formal depth catalog payload file is missing`);
      } else {
        formalDepthByGameId.set(shippedGame.gameId, {
          depthReference: catalog.depth_profiles[0],
          bindingReference: catalog.camera_bindings[0],
          depthProfile: loadJson(depthPath),
          cameraBinding: loadJson(bindingPath),
          depthContentHash: fileSha256(depthPath),
          bindingContentHash: fileSha256(bindingPath),
        });
      }
    }
  }

  const rightsByAppId = new Map(
    Array.isArray(rightsGates?.records)
      ? rightsGates.records.map((record) => [String(record.app_id), record])
      : [],
  );
  const rightsRecordByGameId = new Map();
  for (const rightsRecord of rightsByAppId.values()) {
    for (const gameId of rightsRecord?.catalog_game_ids ?? []) {
      if (shippedById.has(gameId)) rightsRecordByGameId.set(gameId, rightsRecord);
    }
  }
  const finalRecordByGameId = new Map();
  const catalogFinalIdentityConflictGameIds = new Set();
  if (!Array.isArray(finalClassifications?.records) || !Array.isArray(rightsGates?.records)) {
    errors.push("final classifications and rights gates must both contain records arrays");
  } else {
    const expectedCapability = new Map([
      ["adapted", "catalog_ready"],
      ["blocked", "adaptation_blocked"],
      ["not_adaptable", "not_adaptable"],
    ]);
    for (const record of finalClassifications.records) {
      const expected = expectedCapability.get(record.classification);
      if (!expected) {
        errors.push(`unknown final classification for app ${record.app_id}: ${record.classification}`);
        continue;
      }
      const rightsRecord = rightsByAppId.get(String(record.app_id));
      // The final-classification file also contains locally observed games that are
      // intentionally outside the shipped roster and therefore have no rights row.
      if (!rightsRecord) continue;
      for (const gameId of rightsRecord.catalog_game_ids ?? []) {
        if (!shippedById.has(gameId)) continue;
        finalRecordByGameId.set(gameId, record);
        const capabilityRecord = capabilityById.get(gameId);
        const capability = capabilityRecord?.capability;
        const gameProfile = catalogGameProfileByGameId.get(gameId);
        const rawProfileHash = gameProfile?.executable_sha256;
        const profileIdentity = catalogExecutableIdentity(rawProfileHash);
        const profileHash = profileIdentity.hash;
        const classificationHash = normalizeSha256(record.executable?.sha256);
        const classificationBasename = safeWindowsRelativeBasename(
          record.executable?.relative_install_path,
        );
        const catalogExecutableNames = Array.isArray(gameProfile?.executable_names)
          ? gameProfile.executable_names.map((name) => String(name).toLowerCase())
          : [];
        const conflictBlocker = "catalog_final_executable_identity_conflict";
        if (capabilityRecord && gameProfile) {
          if (!classificationHash) {
            errors.push(`${gameId}: final-classification executable SHA-256 is malformed`);
          }
          if (!classificationBasename) {
            errors.push(
              `${gameId}: final-classification executable path must be a safe relative Windows path with a basename`,
            );
          }
          if (profileIdentity.state === "malformed") {
            errors.push(`${gameId}: catalog executable SHA-256 is malformed`);
          }
          const hashConflict =
            profileHash !== null &&
            classificationHash !== null &&
            profileHash !== classificationHash;
          const basenameConflict =
            Boolean(classificationBasename) &&
            !catalogExecutableNames.includes(classificationBasename);
          const conflicted = hashConflict || basenameConflict;
          if (conflicted) catalogFinalIdentityConflictGameIds.add(gameId);
          const declared = capabilityRecord.blockers?.includes(conflictBlocker) === true;
          if (conflicted && !declared) {
            errors.push(
              `${gameId}: catalog and final-classification executable identity conflict; ${conflictBlocker} is required`,
            );
          }
          const identitiesComparable =
            profileHash !== null &&
            classificationHash !== null &&
            classificationBasename !== null;
          if (!conflicted && identitiesComparable && declared) {
            errors.push(`${gameId}: ${conflictBlocker} is stale because both identities match`);
          }
          if (
            conflicted &&
            (capabilityRecord.capability === "catalog_ready" ||
              capabilityRecord.capability === "local_trial" ||
              capabilityRecord.runtimeMode !== null ||
              capabilityRecord.depthCaptureAvailable !== false ||
              capabilityRecord.recordingReady !== false)
          ) {
            errors.push(
              `${gameId}: catalog/final executable identity conflict cannot expose a Depth runtime`,
            );
          }
        }
        const provisionalReasons = finalAuthorityProvisionalReasons(
          record,
          [gameId],
          catalogFinalIdentityConflictGameIds,
        );
        const conservativeProvisionalDowngrade =
          expected === "not_adaptable" &&
          capability === "adaptation_blocked" &&
          provisionalReasons.length > 0;
        if (
          provisionalReasons.includes("product_survey_or_taxonomy_only") &&
          !capabilityRecord?.blockers?.some((blocker) =>
            blocker.includes("requires_exact_build_reconfirmation"),
          )
        ) {
          errors.push(
            `${gameId}: provisional product authority must retain exact-build reconfirmation blocker`,
          );
        }
        if (capability !== expected && !conservativeProvisionalDowngrade) {
          errors.push(
            `${gameId}: final classification ${record.classification} requires ${expected}, got ${capability}`,
          );
        }
        if (
          expected === "not_adaptable" &&
          capability === "not_adaptable" &&
          provisionalReasons.length > 0
        ) {
          errors.push(
            `${gameId}: provisional final-ledger authority cannot support hard-terminal not_adaptable`,
          );
        }
      }
    }
  }
  errors.push(
    ...validateFinalLedgerAudit(
      capabilities.finalLedgerAudit,
      finalClassifications,
      rightsByAppId,
      shippedById,
      catalogFinalIdentityConflictGameIds,
    ),
  );

  const genericTargetByGameId = new Map();
  const genericTargetsPath = join(
    root,
    "config",
    "generic-d3d11-dsv-discovery-targets.json",
  );
  if (existsSync(genericTargetsPath)) {
    try {
      const genericTargets = loadJson(genericTargetsPath);
      for (const target of genericTargets?.targets ?? []) {
        if (typeof target?.gameId === "string") {
          genericTargetByGameId.set(target.gameId, target);
        }
      }
    } catch {
      errors.push("generic D3D11 target registry is not valid JSON");
    }
  }

  const rendererIdentityAuthorityByGameId = new Map();
  const exactInstallAuthorityByGameId = new Map();
  for (const shippedGame of shipped.games) {
    const gameId = shippedGame.gameId;
    let authority = {};
    const rightsRecord = rightsRecordByGameId.get(gameId);
    if (rightsRecord) {
      authority = mergeRendererIdentityAuthority(
        authority,
        { appId: String(rightsRecord.app_id) },
        "rights ledger",
        errors,
        gameId,
      );
    }

    const finalRecord = finalRecordByGameId.get(gameId);
    if (finalRecord) {
      authority = mergeRendererIdentityAuthority(
        authority,
        {
          appId: String(finalRecord.app_id),
          executableBasename: safeWindowsRelativeBasename(
            finalRecord.executable?.relative_install_path,
          ),
          executableSha256: finalRecord.executable?.sha256,
        },
        "final classification",
        errors,
        gameId,
      );
    } else {
      const gameProfile = catalogGameProfileByGameId.get(gameId);
      const executableNames = Array.isArray(gameProfile?.executable_names)
        ? gameProfile.executable_names
        : [];
      if (executableNames.length === 1 && normalizeSha256(gameProfile?.executable_sha256)) {
        authority = mergeRendererIdentityAuthority(
          authority,
          {
            executableBasename: executableNames[0],
            executableSha256: gameProfile.executable_sha256,
          },
          "exact catalog game profile",
          errors,
          gameId,
        );
      }
    }

    const genericTarget = genericTargetByGameId.get(gameId);
    if (genericTarget) {
      authority = mergeRendererIdentityAuthority(
        authority,
        {
          appId: genericTarget.appId,
          buildId: genericTarget.steamBuildId,
          executableBasename: genericTarget.executableBasename,
          executableSha256: genericTarget.executableSha256,
        },
        "generic D3D11 target registry",
        errors,
        gameId,
      );
    }

    const exactInstall = exactInstallIdentityAuthority(root, capabilityById.get(gameId));
    errors.push(...exactInstall.errors);
    if (exactInstall.authority) {
      exactInstallAuthorityByGameId.set(gameId, exactInstall.authority);
      authority = mergeRendererIdentityAuthority(
        authority,
        exactInstall.authority,
        exactInstall.authority.source,
        errors,
        gameId,
      );
    }
    rendererIdentityAuthorityByGameId.set(gameId, authority);
  }

  if (performanceCertifications?.schema !== "dm-depth-performance-certifications-v1") {
    errors.push("performance certifications must use dm-depth-performance-certifications-v1");
  }
  if (
    !Array.isArray(performanceCertifications?.approvedRuntimes) ||
    !Array.isArray(performanceCertifications?.certifications)
  ) {
    errors.push("performance certifications must contain approvedRuntimes and certifications arrays");
  }
  for (const [gameId, formal] of formalDepthByGameId) {
    const historical = retainedPerformancePairs(
      root, capabilityById.get(gameId), finalRecordByGameId.get(gameId),
    );
    errors.push(...historical.errors);
    errors.push(
      ...formalDepthIdentityErrors(
        gameId,
        formal,
        finalRecordByGameId.get(gameId),
        performanceCertifications,
        historical.pairs,
      ),
    );
  }

  for (const game of capabilities.games) {
    const prefix = game?.gameId || "<missing-game-id>";
    if (!shippedById.has(game?.gameId)) continue;
    const exactInstallAuthority = exactInstallAuthorityByGameId.get(game.gameId);
    if (
      exactInstallAuthority?.requiresCleanInstallSurface === true &&
      !game?.blockers?.includes("clean_official_install_surface_missing")
    ) {
      errors.push(
        `${prefix}: exact-install evidence does not prove a clean official install surface; ` +
          "clean_official_install_surface_missing is required",
      );
    }
    for (const key of Object.keys(game)) {
      if (!ALLOWED_GAME_KEYS.has(key)) {
        errors.push(`${prefix} contains unsupported field ${key}`);
      }
    }
    errors.push(
      ...validateRendererEvidence(
        root,
        game,
        rendererIdentityAuthorityByGameId.get(game.gameId),
      ),
    );
    if (!ALLOWED_CAPABILITIES.has(game.capability)) {
      errors.push(`${prefix} has unknown capability ${game.capability}`);
    }
    if (!ALLOWED_EVIDENCE_TIERS.has(game.evidenceTier)) {
      errors.push(`${prefix} has unknown evidenceTier ${game.evidenceTier}`);
    }
    if (typeof game.evidenceLocator !== "string" || game.evidenceLocator.trim().length === 0) {
      errors.push(`${prefix} evidenceLocator must identify the supporting record or survey row`);
    }
    if (
      ["product_survey_only", "product_policy_survey"].includes(game.evidenceTier) &&
      !game.blockers?.some((blocker) => blocker.includes("requires_exact_build_reconfirmation"))
    ) {
      errors.push(`${prefix} product survey evidence must remain blocked on exact-build reconfirmation`);
    }
    if (
      ["product_survey_only", "product_policy_survey"].includes(game.evidenceTier) &&
      game.capability === "not_adaptable"
    ) {
      errors.push(`${prefix} product survey cannot permanently declare not_adaptable`);
    }
    if (game.evidenceTier === "catalog_only" && game.capability !== "not_assessed") {
      errors.push(`${prefix} catalog_only evidence cannot support ${game.capability}`);
    }
    if (
      game.evidenceTier === "exact_build_runtime" &&
      !hasExactRuntimeGateEvidence(root, finalRecordByGameId.get(game.gameId))
    ) {
      errors.push(
        `${prefix} exact_build_runtime requires a repository game-runtime-gate-evidence/v1 record`,
      );
    }
    if (typeof game.depthCaptureAvailable !== "boolean" || typeof game.recordingReady !== "boolean") {
      errors.push(`${prefix} depthCaptureAvailable and recordingReady must be boolean`);
    }
    if (game.recordingReady !== false) {
      errors.push(
        `${prefix} recordingReady is runtime-only and cannot be asserted by this static audit`,
      );
    }
    if (!Array.isArray(game.blockers)) {
      errors.push(`${prefix} blockers must be an array`);
    }
    if (!Array.isArray(game.evidence) || game.evidence.length === 0) {
      errors.push(`${prefix} evidence must contain at least one repository-relative path`);
    } else {
      for (const evidence of game.evidence) {
        if (!evidencePathExists(root, evidence)) {
          errors.push(`${prefix} evidence path is missing or unsafe: ${evidence}`);
        }
      }
    }

    const formalCatalog = catalogsWithFormalDepth.has(game.gameId);
    if (game.historicalPerformancePairs !== undefined && !formalCatalog) {
      errors.push(`${prefix} historicalPerformancePairs requires a formal depth catalog`);
    }
    if (game.capability === "catalog_ready") {
      if (game.evidenceTier !== "formal_catalog") {
        errors.push(`${prefix} catalog_ready must use formal_catalog evidence`);
      }
      if (!formalCatalog) errors.push(`${prefix} claims catalog_ready without a formal depth catalog`);
      if (game.runtimeMode !== "formal_catalog" || !game.depthCaptureAvailable) {
        errors.push(`${prefix} catalog_ready must use formal_catalog and make Depth available`);
      }
      if (game.blockers?.length === 0) {
        errors.push(`${prefix} formal catalog must record its current runtime blockers`);
      }
      const formal = formalDepthByGameId.get(game.gameId);
      const hasCertification =
        formal !== undefined &&
        hasApplicablePerformanceCertification(
          game.gameId,
          formal,
          performanceCertifications,
        );
      const missingCertificationBlocker =
        game.blockers?.includes("approved_runtime_performance_certification_missing") === true;
      if (!hasCertification && !missingCertificationBlocker) {
        errors.push(`${prefix} formal catalog without certification must retain the missing-certification blocker`);
      }
      if (hasCertification && missingCertificationBlocker) {
        errors.push(`${prefix} missing-certification blocker is stale because a certification exists`);
      }
    } else if (game.capability === "local_trial") {
      if (game.evidenceTier !== "exact_build_local_trial") {
        errors.push(`${prefix} local_trial must use exact_build_local_trial evidence`);
      }
      if (formalCatalog) errors.push(`${prefix} local_trial conflicts with a formal depth catalog`);
      if (!game.runtimeMode || !game.depthCaptureAvailable || game.recordingReady) {
        errors.push(`${prefix} local_trial must be available, non-ready, and name its runtime mode`);
      }
      if (!game.blockers?.length) errors.push(`${prefix} local_trial must record promotion blockers`);
    } else {
      if (formalCatalog) errors.push(`${prefix} has a formal depth catalog but capability is ${game.capability}`);
      if (game.runtimeMode !== null || game.depthCaptureAvailable || game.recordingReady) {
        errors.push(`${prefix} ${game.capability} must remain disabled with no runtime mode`);
      }
      if (!game.blockers?.length) errors.push(`${prefix} ${game.capability} must record blockers`);
    }
  }

  if (!Array.isArray(capabilities.pendingOnboarding)) {
    errors.push("pendingOnboarding must be an array");
  } else {
    const pendingIds = capabilities.pendingOnboarding.map((game) => game?.gameId);
    const pendingAppIds = capabilities.pendingOnboarding.map((game) => String(game?.appId ?? ""));
    for (const duplicate of duplicateValues(pendingIds)) {
      errors.push(`duplicate pending onboarding gameId: ${duplicate}`);
    }
    for (const duplicate of duplicateValues(pendingAppIds)) {
      errors.push(`duplicate pending onboarding appId: ${duplicate}`);
    }
    for (const pending of capabilities.pendingOnboarding) {
      const prefix = pending?.gameId || "<missing-pending-game-id>";
      if (pending === null || typeof pending !== "object" || Array.isArray(pending)) {
        errors.push(`${prefix} pendingOnboarding entry must be an object`);
        continue;
      }
      for (const key of Object.keys(pending)) {
        if (!ALLOWED_PENDING_ONBOARDING_KEYS.has(key)) {
          errors.push(`${prefix} pendingOnboarding contains unsupported field ${key}`);
        }
      }
      if (
        typeof pending.gameId !== "string" ||
        pending.gameId.length === 0 ||
        pending.gameId.length > 128 ||
        !/^[a-z0-9][a-z0-9-]*$/.test(pending.gameId) ||
        typeof pending.displayName !== "string" ||
        pending.displayName.length === 0 ||
        pending.displayName.length > 256 ||
        pending.displayName.trim() !== pending.displayName ||
        /[\u0000-\u001f\u007f]/.test(pending.displayName)
      ) {
        errors.push(`${prefix} pending onboarding identity is malformed`);
      }
      if (shippedById.has(pending?.gameId)) errors.push(`${prefix} cannot be pending and shipped`);
      if (!/^\d+$/.test(pending?.appId ?? "") || !/^\d+$/.test(pending?.targetBuildId ?? "")) {
        errors.push(`${prefix} must pin decimal appId and targetBuildId`);
      }
      if (!Number.isInteger(pending?.steamStateFlags)) {
        errors.push(`${prefix} steamStateFlags must be an integer`);
      }
      if (pending?.catalogAuthored !== false) {
        errors.push(`${prefix} pending onboarding must remain unauthored`);
      }
      if (!Array.isArray(pending?.blockers) || pending.blockers.length === 0) {
        errors.push(`${prefix} pending onboarding must record blockers`);
      } else if (
        pending.blockers.some(
          (blocker) => typeof blocker !== "string" || !/^[a-z0-9][a-z0-9_]*$/.test(blocker),
        ) ||
        new Set(pending.blockers).size !== pending.blockers.length
      ) {
        errors.push(`${prefix} pending onboarding blockers must be unique canonical strings`);
      }
      for (const runtimeOnlyField of ["runtimeMode", "depthCaptureAvailable", "recordingReady"]) {
        if (Object.hasOwn(pending, runtimeOnlyField)) {
          errors.push(
            `${prefix} pending onboarding must not declare static Depth runtime field ${runtimeOnlyField}`,
          );
        }
      }
      if (pending?.capability === "install_blocked") {
        if (pending.steamStateFlags === 4) {
          errors.push(`${prefix} install_blocked conflicts with complete Steam state`);
        }
        if (!pending.blockers?.includes("steam_install_incomplete")) {
          errors.push(`${prefix} install_blocked must explicitly record steam_install_incomplete`);
        }
        if (pending.executableCandidate !== null) {
          errors.push(`${prefix} install_blocked must keep executableCandidate null`);
        }
        for (const unavailableRuntimeField of [
          "executableSha256",
          "graphicsApi",
          "runtimeEvidence",
          "localProfileEvidence",
        ]) {
          if (Object.hasOwn(pending, unavailableRuntimeField)) {
            errors.push(
              `${prefix} install_blocked must not declare unavailable runtime field ${unavailableRuntimeField}`,
            );
          }
        }
      } else if (pending?.capability === "runtime_onboarding_pending") {
        if (pending.steamStateFlags !== 4) {
          errors.push(`${prefix} runtime_onboarding_pending requires complete Steam state`);
        }
        if (pending.blockers?.includes("steam_install_incomplete")) {
          errors.push(`${prefix} runtime_onboarding_pending contradicts steam_install_incomplete`);
        }
        const executableCandidate = pending.executableCandidate;
        if (
          typeof executableCandidate !== "string" ||
          safeWindowsRelativeBasename(executableCandidate) !== executableCandidate.toLowerCase() ||
          !executableCandidate.toLowerCase().endsWith(".exe")
        ) {
          errors.push(
            `${prefix} runtime_onboarding_pending must name one safe executable basename`,
          );
        }
        if (!/^sha256:[0-9a-f]{64}$/.test(pending.executableSha256 ?? "")) {
          errors.push(`${prefix} runtime_onboarding_pending must pin lowercase executable SHA-256`);
        }
        if (!["d3d11", "d3d12", "vulkan"].includes(pending.graphicsApi)) {
          errors.push(`${prefix} runtime_onboarding_pending must record a verified graphics API`);
        }
        const runtimeEvidence = pending.runtimeEvidence;
        if (
          runtimeEvidence === null ||
          typeof runtimeEvidence !== "object" ||
          Array.isArray(runtimeEvidence)
        ) {
          errors.push(
            `${prefix} runtime_onboarding_pending requires a structured runtimeEvidence summary`,
          );
        } else {
          const allowedRuntimeEvidenceKeys = new Set([
            "schema",
            "appId",
            "targetBuildId",
            "executableBasename",
            "executableSha256",
            "graphicsApi",
            "launches",
          ]);
          for (const key of Object.keys(runtimeEvidence)) {
            if (!allowedRuntimeEvidenceKeys.has(key)) {
              errors.push(`${prefix} runtimeEvidence contains unsupported field ${key}`);
            }
          }
          if (runtimeEvidence.schema !== "dm-pending-runtime-evidence-v1") {
            errors.push(`${prefix} runtimeEvidence schema is unsupported`);
          }
          for (const [field, expected] of [
            ["appId", String(pending.appId ?? "")],
            ["targetBuildId", String(pending.targetBuildId ?? "")],
            ["executableBasename", pending.executableCandidate],
            ["executableSha256", pending.executableSha256],
            ["graphicsApi", pending.graphicsApi],
          ]) {
            if (runtimeEvidence[field] !== expected) {
              errors.push(`${prefix} runtimeEvidence ${field} must match the pending identity`);
            }
          }
          const launches = runtimeEvidence.launches;
          const allowedLaunchKeys = new Set([
            "ordinal",
            "sourceReportSha256",
            "sceneKind",
            "processIdentityMatched",
            "graphicsApi",
            "runtimeProtectionGate",
            "window",
          ]);
          const allowedWindowKeys = new Set(["className", "clientWidth", "clientHeight"]);
          if (!Array.isArray(launches) || launches.length === 0) {
            errors.push(`${prefix} runtimeEvidence launches must be a non-empty array`);
          }
          const reportHashes = [];
          const windowClasses = [];
          let hasRepresentativeScene = false;
          for (const [index, launch] of (Array.isArray(launches) ? launches : []).entries()) {
            const launchPrefix = `${prefix} runtimeEvidence launch ${index + 1}`;
            if (launch === null || typeof launch !== "object" || Array.isArray(launch)) {
              errors.push(`${launchPrefix} must be an object`);
              continue;
            }
            for (const key of Object.keys(launch)) {
              if (!allowedLaunchKeys.has(key)) {
                errors.push(`${launchPrefix} contains unsupported field ${key}`);
              }
            }
            if (launch.ordinal !== index + 1) {
              errors.push(`${launchPrefix} ordinal must be continuous from one`);
            }
            if (!/^sha256:[0-9a-f]{64}$/.test(launch.sourceReportSha256 ?? "")) {
              errors.push(`${launchPrefix} source report SHA-256 is malformed`);
            } else {
              reportHashes.push(launch.sourceReportSha256);
            }
            if (
              !["display_calibration", "online_policy", "main_menu", "representative_single_player"].includes(
                launch.sceneKind,
              )
            ) {
              errors.push(`${launchPrefix} sceneKind is unsupported`);
            }
            if (launch.sceneKind === "representative_single_player") {
              hasRepresentativeScene = true;
            }
            if (launch.processIdentityMatched !== true) {
              errors.push(`${launchPrefix} process identity must be matched`);
            }
            if (launch.graphicsApi !== pending.graphicsApi) {
              errors.push(`${launchPrefix} graphicsApi must match the pending identity`);
            }
            if (launch.runtimeProtectionGate !== "pass") {
              errors.push(`${launchPrefix} runtime protection gate must pass`);
            }
            const window = launch.window;
            if (window === null || typeof window !== "object" || Array.isArray(window)) {
              errors.push(`${launchPrefix} window must be an object`);
              continue;
            }
            for (const key of Object.keys(window)) {
              if (!allowedWindowKeys.has(key)) {
                errors.push(`${launchPrefix} window contains unsupported field ${key}`);
              }
            }
            if (
              typeof window.className !== "string" ||
              window.className.length === 0 ||
              window.className.length > 256 ||
              window.className.trim() !== window.className ||
              /[\u0000-\u001f\u007f]/.test(window.className) ||
              !Number.isInteger(window.clientWidth) ||
              window.clientWidth < 1 ||
              !Number.isInteger(window.clientHeight) ||
              window.clientHeight < 1
            ) {
              errors.push(`${launchPrefix} window identity is malformed`);
            } else {
              windowClasses.push(window.className);
            }
          }
          if (new Set(reportHashes).size !== reportHashes.length) {
            errors.push(`${prefix} runtimeEvidence source report SHA-256 values must be unique`);
          }
          const representativeBlocker =
            pending.blockers?.includes(
              "representative_single_player_runtime_evidence_missing",
            ) === true;
          if (!hasRepresentativeScene && !representativeBlocker) {
            errors.push(
              `${prefix} non-representative runtimeEvidence must retain representative scene blocker`,
            );
          }
          if (hasRepresentativeScene && representativeBlocker) {
            errors.push(
              `${prefix} representative scene blocker is stale for representative runtimeEvidence`,
            );
          }
          const secondLaunchBlocker =
            pending.blockers?.includes("second_launch_window_identity_missing") === true;
          const inconsistentWindowBlocker =
            pending.blockers?.includes("window_identity_inconsistent") === true;
          const hasTwoWindows = windowClasses.length >= 2;
          const hasStableSecondWindow =
            hasTwoWindows && windowClasses.every((className) => className === windowClasses[0]);
          if (!hasTwoWindows && !secondLaunchBlocker) {
            errors.push(
              `${prefix} one runtime launch must retain the second-launch window blocker`,
            );
          }
          if (hasTwoWindows && secondLaunchBlocker) {
            errors.push(`${prefix} second-launch window blocker is stale`);
          }
          if (hasStableSecondWindow && inconsistentWindowBlocker) {
            errors.push(`${prefix} window-identity-inconsistent blocker is stale`);
          }
          if (hasTwoWindows && !hasStableSecondWindow && !inconsistentWindowBlocker) {
            errors.push(`${prefix} inconsistent window classes require an explicit blocker`);
          }
        }
        if (pending.gameId === MAFIA3_PENDING_GAME_ID) {
          errors.push(...validatePendingLocalProfileEvidence(pending.localProfileEvidence, pending));
          const physicalMappingBlocker =
            "physical_input_mapping_and_semantic_authoring_missing";
          const localProfileHasPhysicalMappingBlocker =
            pending.localProfileEvidence?.blockers?.includes(physicalMappingBlocker) === true;
          const pendingHasPhysicalMappingBlocker =
            pending.blockers?.includes(physicalMappingBlocker) === true;
          if (localProfileHasPhysicalMappingBlocker !== pendingHasPhysicalMappingBlocker) {
            errors.push(
              `${prefix} localProfileEvidence and pending blockers must agree on ${physicalMappingBlocker}`,
            );
          }
          for (const staleProfileBlocker of [
            "display_mode_and_sdr_confirmation_missing",
            "local_input_mapping_evidence_missing",
            "video_config_evidence_missing",
            "input_profile_evidence_missing",
          ]) {
            if (pending.blockers?.includes(staleProfileBlocker)) {
              errors.push(
                `${prefix} localProfileEvidence makes blocker stale: ${staleProfileBlocker}`,
              );
            }
          }
          for (const requiredProfileBlocker of [
            "hdr_or_sdr_confirmation_missing",
            "input_profile_default_preset_confirmation_missing",
            "physical_input_mapping_and_semantic_authoring_missing",
          ]) {
            if (!pending.blockers?.includes(requiredProfileBlocker)) {
              errors.push(
                `${prefix} localProfileEvidence requires current blocker ${requiredProfileBlocker}`,
              );
            }
          }
        } else if (Object.hasOwn(pending, "localProfileEvidence")) {
          errors.push(
            `${prefix} localProfileEvidence is reserved for the Mafia III schema discriminator`,
          );
        }
        for (const staleBlocker of [
          "game_executable_missing",
          "stable_executable_hash_missing",
          "shipping_process_identity_missing",
          "graphics_api_runtime_unverified",
        ]) {
          if (pending.blockers?.includes(staleBlocker)) {
            errors.push(`${prefix} runtime_onboarding_pending retains stale blocker ${staleBlocker}`);
          }
        }
        for (const requiredBlocker of [
          "d7_rgb_runtime_evidence_missing",
          "depth_profile_missing",
          "camera_binding_missing",
        ]) {
          if (!pending.blockers?.includes(requiredBlocker)) {
            errors.push(
              `${prefix} runtime_onboarding_pending must retain ${requiredBlocker}`,
            );
          }
        }
      } else {
        errors.push(`${prefix} has unsupported pending capability ${pending?.capability}`);
      }
      const pendingRights = rightsByAppId.get(String(pending?.appId));
      if (!pendingRights) {
        errors.push(`${prefix} pending AppID has no rights-ledger record`);
      } else {
        if (pendingRights.decision !== "counsel_approved") {
          errors.push(`${prefix} pending rights-ledger record is not approved`);
        }
        if (!Array.isArray(pendingRights.catalog_game_ids) || pendingRights.catalog_game_ids.length !== 0) {
          errors.push(`${prefix} pending rights-ledger record must keep catalog_game_ids empty`);
        }
      }
      if (!pending?.blockers?.includes("rights_ledger_catalog_link_pending")) {
        errors.push(`${prefix} must explicitly record rights_ledger_catalog_link_pending`);
      }
      if (pending?.blockers?.includes("rights_ledger_record_missing")) {
        errors.push(`${prefix} contradicts the existing pending rights-ledger record`);
      }
      if (!Array.isArray(pending?.evidence) || pending.evidence.length === 0) {
        errors.push(`${prefix} evidence must contain at least one repository-relative path`);
      } else if (
        pending.evidence.some((evidence) => typeof evidence !== "string") ||
        new Set(pending.evidence).size !== pending.evidence.length
      ) {
        errors.push(`${prefix} evidence paths must be unique strings`);
      }
      for (const evidence of pending?.evidence ?? []) {
        if (!evidencePathExists(root, evidence)) {
          errors.push(`${prefix} evidence path is missing or unsafe: ${evidence}`);
        }
      }
    }
  }
  return errors;
}

function selfTest(
  capabilities,
  shipped,
  finalClassifications,
  rightsGates,
  performanceCertifications,
) {
  selfTestRendererEvidenceFixtures();
  if (normalizeSha256(`SHA256:${"A".repeat(64)}`) !== "a".repeat(64)) {
    throw new Error("self-test SHA-256 normalization rejected an equivalent prefix/case form");
  }
  if (catalogExecutableIdentity(null).state !== "unbound") {
    throw new Error("self-test rejected an explicit null catalog executable identity");
  }
  for (const malformedProfileHash of [undefined, "", "not-a-sha256"]) {
    if (catalogExecutableIdentity(malformedProfileHash).state !== "malformed") {
      throw new Error("self-test accepted a missing or malformed catalog executable identity");
    }
  }

  const gtaExactInstallPath = join(
    ROOT,
    "docs",
    "evidence",
    "gta5-legacy-exact-install-20260902.json",
  );
  const gtaExactInstall = loadJson(gtaExactInstallPath);
  const exactInstallBaseline = validateExactInstallIdentityRecord(
    gtaExactInstall,
    "gta5-legacy-pc",
    "docs/evidence/gta5-legacy-exact-install-20260902.json",
  );
  if (exactInstallBaseline.errors.length > 0 || !exactInstallBaseline.authority) {
    throw new Error(
      `self-test rejected GTA exact-install identity: ${exactInstallBaseline.errors.join("; ")}`,
    );
  }
  if (exactInstallBaseline.authority.requiresCleanInstallSurface !== true) {
    throw new Error("self-test exact-install authority did not retain the clean-install blocker");
  }
  for (const [name, mutateRecord, expected] of [
    [
      "exact-install target build drift",
      (record) => {
        record.steam_install_gate.target_build_id = "24129524";
      },
      "target_build_id must equal build_id",
    ],
    [
      "exact-install launched observation",
      (record) => {
        record.safety_boundary.game_launched_for_this_observation = true;
      },
      "safety_boundary must remain read-only, sanitized, and no-bypass",
    ],
    [
      "exact-install executable traversal",
      (record) => {
        record.shipping_executable.relative_path = "..\\GTA5.exe";
      },
      "shipping_executable.relative_path must be a safe relative executable path",
    ],
    [
      "exact-install runtime overclaim",
      (record) => {
        record.runtime_admission_state.recording_ready = true;
      },
      "must not authorize D6, D7, Depth capture, or recording",
    ],
    [
      "exact-install window overclaim",
      (record) => {
        record.catalog_update.window_class = "FixtureWindow";
      },
      "must not claim a window, DepthProfile, or CameraBinding",
    ],
    [
      "exact-install manifest AppID drift",
      (record) => {
        record.steam_install_gate.manifest_snapshot.relative_path =
          "steamapps/appmanifest_271591.acf";
      },
      "manifest relative_path must canonically match the evidence AppID",
    ],
    [
      "exact-install empty manifest",
      (record) => {
        record.steam_install_gate.manifest_snapshot.length_bytes = 0;
      },
      "manifest length_bytes must be a positive safe integer",
    ],
    [
      "exact-install noncanonical manifest hash",
      (record) => {
        record.steam_install_gate.manifest_snapshot.sha256 =
          record.steam_install_gate.manifest_snapshot.sha256.toUpperCase();
      },
      "manifest sha256 must use canonical sha256:<lowercase-hex> form",
    ],
    [
      "exact-install zero stability interval",
      (record) => {
        record.steam_install_gate.stability_interval_seconds = 0;
      },
      "stability_interval_seconds must be a positive safe integer",
    ],
    [
      "exact-install rights AppID drift",
      (record) => {
        record.rights_gate_observation.app_id = "271591";
      },
      "rights_gate_observation.app_id must equal game.steam_app_id",
    ],
    [
      "exact-install anti-cheat gate overclaim",
      (record) => {
        record.rights_gate_observation.runtime_anti_cheat_gate_still_required = false;
      },
      "rights_gate_observation must keep the runtime anti-cheat gate required",
    ],
    [
      "exact-install pristine-tree overclaim",
      (record) => {
        record.runtime_admission_state.install_tree_pristine = true;
      },
      "runtime_admission_state.install_tree_pristine must remain not_asserted",
    ],
  ]) {
    const driftedRecord = structuredClone(gtaExactInstall);
    mutateRecord(driftedRecord);
    const drifted = validateExactInstallIdentityRecord(
      driftedRecord,
      "gta5-legacy-pc",
      "docs/evidence/gta5-legacy-exact-install-20260902.json",
    );
    if (!drifted.errors.some((error) => error.includes(expected))) {
      throw new Error(
        `self-test ${name} did not fail closed: ${drifted.errors.join("; ")}`,
      );
    }
  }

  const falloutShipped = shipped.games.find(
    (entry) => entry.gameId === "fallout4-steam-release-test",
  );
  const falloutRoot = join(ROOT, "config", "catalogs", "dev", falloutShipped.catalogDir);
  const falloutCatalog = loadJson(join(falloutRoot, "catalog.json"));
  const falloutGameProfile = loadJson(join(falloutRoot, "game-profile.json"));
  if (falloutGameProfile.executable_sha256 !== null) {
    throw new Error("self-test requires the formal Fallout catalog's game-profile identity to remain unbound");
  }
  const falloutFormal = {
    depthReference: falloutCatalog.depth_profiles[0],
    bindingReference: falloutCatalog.camera_bindings[0],
    depthProfile: loadJson(join(falloutRoot, "depth-profile.json")),
    cameraBinding: loadJson(join(falloutRoot, "camera-binding.json")),
    depthContentHash: fileSha256(join(falloutRoot, "depth-profile.json")),
    bindingContentHash: fileSha256(join(falloutRoot, "camera-binding.json")),
  };
  const falloutFinal = finalClassifications.records.find(
    (entry) => String(entry.app_id) === "377160",
  );
  const baselineFormalErrors = formalDepthIdentityErrors(
    "fallout4-steam-release-test",
    falloutFormal,
    falloutFinal,
    performanceCertifications,
  );
  if (baselineFormalErrors.length > 0) {
    throw new Error(`self-test rejected the exact Fallout formal pair: ${baselineFormalErrors.join("; ")}`);
  }
  selfTestHistoricalPerformancePairs(falloutFormal, falloutFinal);
  const formalDriftCases = [
    [
      "binding executable",
      (formal) => {
        formal.cameraBinding.executable_sha256 = `sha256:${"b".repeat(64)}`;
      },
      "DepthProfile and CameraBinding executable identities differ",
    ],
    [
      "manifest content hash",
      (formal) => {
        formal.depthReference.content_hash = `sha256:${"b".repeat(64)}`;
      },
      "DepthProfile manifest content hash does not match payload bytes",
    ],
  ];
  for (const [name, mutateFormal, expected] of formalDriftCases) {
    const driftedFormal = structuredClone(falloutFormal);
    mutateFormal(driftedFormal);
    const driftErrors = formalDepthIdentityErrors(
      "fallout4-steam-release-test",
      driftedFormal,
      falloutFinal,
      performanceCertifications,
    );
    if (!driftErrors.some((error) => error.includes(expected))) {
      throw new Error(`self-test ${name} drift did not fail closed: ${driftErrors.join("; ")}`);
    }
  }
  const driftedFalloutFinal = structuredClone(falloutFinal);
  driftedFalloutFinal.executable.sha256 = "b".repeat(64);
  const finalPairDriftErrors = formalDepthIdentityErrors(
    "fallout4-steam-release-test",
    falloutFormal,
    driftedFalloutFinal,
    performanceCertifications,
  );
  if (!finalPairDriftErrors.some((error) => error.includes("differs from final classification"))) {
    throw new Error(
      `self-test formal pair/final identity drift did not fail closed: ${finalPairDriftErrors.join("; ")}`,
    );
  }
  const driftedCertifications = structuredClone(performanceCertifications);
  driftedCertifications.certifications.push({
    certificationId: "self-test-stale-fallout-pair",
    key: {
      gameId: "fallout4-steam-release-test",
      depthProfileSha256: `sha256:${"b".repeat(64)}`,
      cameraBindingSha256: falloutFormal.bindingReference.content_hash,
    },
  });
  const certificationDriftErrors = formalDepthIdentityErrors(
    "fallout4-steam-release-test",
    falloutFormal,
    falloutFinal,
    driftedCertifications,
  );
  if (!certificationDriftErrors.some((error) => error.includes("does not bind the exact formal"))) {
    throw new Error(
      `self-test stale formal-pair certification did not fail closed: ${certificationDriftErrors.join("; ")}`,
    );
  }
  const applicableCertifications = {
    schema: "dm-depth-performance-certifications-v1",
    approvedRuntimes: [{ d3d11Generation: 4, d3d12Generation: 9 }],
    certifications: [
      {
        certificationId: "self-test-current-fallout-pair",
        key: {
          gameId: "fallout4-steam-release-test",
          rendererBackend: "d3d11",
          backendGeneration: 4,
          depthProfileSha256: falloutFormal.depthReference.content_hash,
          cameraBindingSha256: falloutFormal.bindingReference.content_hash,
        },
      },
    ],
  };
  if (
    !hasApplicablePerformanceCertification(
      "fallout4-steam-release-test",
      falloutFormal,
      applicableCertifications,
    )
  ) {
    throw new Error("self-test rejected a generation-approved certification for the exact formal pair");
  }
  applicableCertifications.approvedRuntimes[0].d3d11Generation = 5;
  if (
    hasApplicablePerformanceCertification(
      "fallout4-steam-release-test",
      falloutFormal,
      applicableCertifications,
    )
  ) {
    throw new Error("self-test accepted a formal-pair certification without an approved runtime generation");
  }
  const tests = [];
  const mutate = (callback) => {
    const copy = structuredClone(capabilities);
    callback(copy);
    return copy;
  };
  tests.push([
    "missing shipped game",
    mutate((copy) => copy.games.pop()),
    "shipped game has no depth capability state",
  ]);
  tests.push([
    "stale capability schema",
    mutate((copy) => {
      copy.schemaVersion = "dm-game-depth-capabilities-2";
    }),
    "schemaVersion must be dm-game-depth-capabilities-3",
  ]);
  tests.push([
    "missing renderer evidence",
    mutate((copy) => {
      delete copy.games[0].rendererEvidence;
    }),
    "rendererEvidence must be an object",
  ]);
  tests.push([
    "unsupported game field",
    mutate((copy) => {
      copy.games[0].graphicsApi = "d3d12";
    }),
    "contains unsupported field graphicsApi",
  ]);
  tests.push([
    "unsupported renderer field",
    mutate((copy) => {
      copy.games[0].rendererEvidence.runtimeVerified = true;
    }),
    "rendererEvidence contains unsupported field runtimeVerified",
  ]);
  tests.push([
    "unsupported renderer API",
    mutate((copy) => {
      copy.games[0].rendererEvidence.graphicsApis = ["opengl"];
    }),
    "rendererEvidence has unsupported graphics API opengl",
  ]);
  tests.push([
    "unsupported renderer evidence tier",
    mutate((copy) => {
      copy.games[0].rendererEvidence.apiEvidenceTier = "runtime_guess";
    }),
    "rendererEvidence has unsupported apiEvidenceTier runtime_guess",
  ]);
  tests.push([
    "unknown renderer tier drift",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "avatar-frontiers-of-pandora-pc",
      );
      game.rendererEvidence.apiEvidenceTier = "product_survey";
    }),
    "unknown renderer must use unknown apiEvidenceTier",
  ]);
  tests.push([
    "duplicate renderer API",
    mutate((copy) => {
      const game = copy.games.find((entry) => entry.gameId === "red-dead-redemption-2-pc");
      game.rendererEvidence.graphicsApis = ["d3d12", "d3d12"];
    }),
    "rendererEvidence graphicsApis must not contain duplicates",
  ]);
  tests.push([
    "non-canonical renderer API order",
    mutate((copy) => {
      const game = copy.games.find((entry) => entry.gameId === "red-dead-redemption-2-pc");
      game.rendererEvidence.graphicsApis = ["vulkan", "d3d12"];
    }),
    "rendererEvidence graphicsApis must use canonical order",
  ]);
  tests.push([
    "renderer mode mismatch",
    mutate((copy) => {
      copy.games[0].rendererEvidence.rendererMode = "multi";
    }),
    "does not match graphicsApis (single)",
  ]);
  tests.push([
    "unknown renderer evidence overclaim",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "avatar-frontiers-of-pandora-pc",
      );
      game.rendererEvidence.evidence = {
        path: "docs/143-seven-local-games-rgb-v1-scope.md",
        locator: "§3.1",
      };
    }),
    "unknown renderer must keep renderer evidence null",
  ]);
  tests.push([
    "known renderer missing evidence",
    mutate((copy) => {
      copy.games[0].rendererEvidence.evidence = null;
    }),
    "known renderer API requires a structured evidence locator",
  ]);
  tests.push([
    "exact renderer missing executable identity",
    mutate((copy) => {
      delete copy.games.find(
        (entry) => entry.gameId === "assassins-creed-iii-remastered-steam",
      ).rendererEvidence.exactBuildIdentity;
    }),
    "exact renderer evidence requires exactBuildIdentity",
  ]);
  tests.push([
    "product survey smuggles exact executable identity",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "doom-eternal-pc",
      ).rendererEvidence.exactBuildIdentity = {
        appId: "782330",
        buildId: null,
        executableBasename: "DOOMEternalx64vk.exe",
        executableSha256: `sha256:${"a".repeat(64)}`,
      };
    }),
    "non-exact renderer evidence must not declare an exact-build identity",
  ]);
  tests.push([
    "exact renderer executable hash drifts from final classification",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      ).rendererEvidence.exactBuildIdentity.executableSha256 = `sha256:${"a".repeat(64)}`;
    }),
    "exactBuildIdentity executableSha256 differs from final classification",
  ]);
  tests.push([
    "AC3 exact renderer build drifts from target registry",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "assassins-creed-iii-remastered-steam",
      ).rendererEvidence.exactBuildIdentity.buildId = "11324512";
    }),
    "exactBuildIdentity buildId differs from generic D3D11 target registry",
  ]);
  tests.push([
    "GTA exact renderer build drifts from exact-install evidence",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "gta5-legacy-pc",
      ).rendererEvidence.exactBuildIdentity.buildId = "24129524";
    }),
    "exactBuildIdentity buildId differs from docs/evidence/gta5-legacy-exact-install-20260902.json",
  ]);
  tests.push([
    "GTA drops the unresolved clean-install-surface blocker",
    mutate((copy) => {
      const gta = copy.games.find((entry) => entry.gameId === "gta5-legacy-pc");
      gta.blockers = gta.blockers.filter(
        (blocker) => blocker !== "clean_official_install_surface_missing",
      );
    }),
    "clean_official_install_surface_missing is required",
  ]);
  tests.push([
    "CP2077 runtime report cannot claim a build id it does not carry",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "cyberpunk-2077-steam",
      ).rendererEvidence.exactBuildIdentity.buildId = "20383526";
    }),
    "exactBuildIdentity buildId differs from available authority",
  ]);
  tests.push([
    "renderer evidence path escape",
    mutate((copy) => {
      copy.games[0].rendererEvidence.evidence.path = "../outside.md";
    }),
    "renderer evidence path is missing or unsafe",
  ]);
  tests.push([
    "renderer evidence list detachment",
    mutate((copy) => {
      const game = copy.games.find((entry) => entry.gameId === "doom-eternal-pc");
      game.evidence = game.evidence.filter(
        (path) => path !== game.rendererEvidence.evidence.path,
      );
    }),
    "renderer evidence path must also appear in the game evidence list",
  ]);
  tests.push([
    "survey promoted to exact renderer evidence",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "deathloop-pc",
      ).rendererEvidence.apiEvidenceTier = "exact_build_observation";
    }),
    "exact_build_observation renderer evidence conflicts with product_survey_only",
  ]);
  tests.push([
    "runtime report renderer mismatch",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "bioshock-infinite-steam-e2e",
      ).rendererEvidence.graphicsApis = ["d3d12"];
    }),
    "renderer API does not match exact runtime report graphics.expected_api",
  ]);
  tests.push([
    "AC3 renderer observation replaced by Hades roster entry",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "assassins-creed-iii-remastered-steam",
      );
      const substitutedPath = "docs/114-dm-capture-host-game-adaptation-roster.md";
      game.rendererEvidence.evidence = {
        path: substitutedPath,
        locator: "exact installed-build table / AppID 1145350 Renderer=DirectX 12",
      };
      game.evidence.push(substitutedPath);
    }),
    "exact renderer evidence bytes do not contain the bound executable hash",
  ]);
  tests.push([
    "runtime report schema substitution",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "bioshock-infinite-steam-e2e",
      ).rendererEvidence.evidence = {
        path: "config/game-depth-final-classifications.json",
        locator: "records[app_id=8870]",
      };
    }),
    "exact runtime renderer evidence has the wrong report schema",
  ]);
  tests.push([
    "enabled capability renderer evidence downgrade",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "fallout4-steam-release-test",
      ).rendererEvidence.apiEvidenceTier = "product_survey";
    }),
    "enabled Depth capability requires exact-build runtime renderer evidence",
  ]);
  tests.push([
    "local trial renderer tier detachment",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "cyberpunk-2077-steam",
      ).rendererEvidence.apiEvidenceTier = "exact_build_local_trial";
    }),
    "exact_build_local_trial renderer evidence requires one local_trial API",
  ]);
  tests.push([
    "false ready claim",
    mutate((copy) => {
      const game = copy.games.find((entry) => entry.capability === "not_assessed");
      game.recordingReady = true;
    }),
    "recordingReady is runtime-only",
  ]);
  tests.push([
    "local trial promotion",
    mutate((copy) => {
      // No shipped game is a local trial any more (CP2077 was promoted 2026-09-18), so
      // demote one to a local-trial shape and let it claim readiness.
      const game = copy.games.find((entry) => entry.gameId === "cyberpunk-2077-steam");
      game.capability = "local_trial";
      game.evidenceTier = "exact_build_local_trial";
      game.rendererEvidence.apiEvidenceTier = "exact_build_local_trial";
      game.runtimeMode = "local_trial_fixture";
      game.recordingReady = true;
    }),
    "must be available, non-ready",
  ]);
  tests.push([
    "final classification drift",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "bioshock-infinite-steam-e2e",
      );
      game.capability = "not_assessed";
    }),
    "final classification blocked requires adaptation_blocked",
  ]);
  tests.push([
    "non-terminal observed classification drift",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "euro-truck-simulator-2-steam",
      );
      game.capability = "not_assessed";
    }),
    "final classification blocked requires adaptation_blocked",
  ]);
  tests.push([
    "survey evidence overclaim",
    mutate((copy) => {
      const game = copy.games.find((entry) => entry.gameId === "deathloop-pc");
      game.blockers = game.blockers.filter(
        (blocker) => !blocker.includes("requires_exact_build_reconfirmation"),
      );
    }),
    "product survey evidence must remain blocked on exact-build reconfirmation",
  ]);
  tests.push([
    "product policy terminal overclaim",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "call-of-duty-steam-release-test",
      ).capability = "not_adaptable";
    }),
    "product survey cannot permanently declare not_adaptable",
  ]);
  tests.push([
    "final ledger total conflated with shipped roster",
    mutate((copy) => {
      copy.finalLedgerAudit.ledgerTotalRecords = copy.finalLedgerAudit.shippedRosterGames;
    }),
    `finalLedgerAudit ledgerTotalRecords must be ${finalClassifications.records.length}`,
  ]);
  tests.push([
    "missing shipped mapping from final ledger audit",
    mutate((copy) => {
      copy.finalLedgerAudit.mappedRecords = copy.finalLedgerAudit.mappedRecords.filter(
        (record) => record.appId !== "1145350",
      );
    }),
    "missing mapped final record for AppID 1145350",
  ]);
  tests.push([
    "product-taxonomy final flag treated as hard-terminal authority",
    mutate((copy) => {
      const mapping = copy.finalLedgerAudit.mappedRecords.find(
        (record) => record.appId === "1145350",
      );
      mapping.authorityMaturity = "hard_terminal_for_observed_build";
      mapping.provisionalReasons = [];
    }),
    "AppID 1145350 authorityMaturity must be provisional",
  ]);
  tests.push([
    "provisional Hades authority promoted to hard terminal capability",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      ).capability = "not_adaptable";
    }),
    "provisional final-ledger authority cannot support hard-terminal not_adaptable",
  ]);
  tests.push([
    "provisional Hades product authority loses reconfirmation blocker",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      );
      game.blockers = game.blockers.filter(
        (blocker) => !blocker.includes("requires_exact_build_reconfirmation"),
      );
    }),
    "provisional product authority must retain exact-build reconfirmation blocker",
  ]);
  tests.push([
    "identity-conflicted final record treated as hard-terminal authority",
    mutate((copy) => {
      const mapping = copy.finalLedgerAudit.mappedRecords.find(
        (record) => record.appId === "261550",
      );
      mapping.authorityMaturity = "hard_terminal_for_observed_build";
      mapping.provisionalReasons = [];
    }),
    "AppID 261550 authorityMaturity must be provisional",
  ]);
  tests.push([
    "taxonomy evidence tier overclaim",
    mutate((copy) => {
      copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      ).evidenceTier = "exact_build_runtime";
    }),
    "exact_build_runtime requires a repository game-runtime-gate-evidence/v1 record",
  ]);
  tests.push([
    "unacknowledged exact-build identity mismatch",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "mount-and-blade-2-bannerlord-steam",
      );
      game.blockers = game.blockers.filter(
        (blocker) => blocker !== "catalog_final_executable_identity_conflict",
      );
    }),
    "catalog and final-classification executable identity conflict",
  ]);
  tests.push([
    "second unacknowledged exact-build identity mismatch",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      );
      game.blockers = game.blockers.filter(
        (blocker) => blocker !== "catalog_final_executable_identity_conflict",
      );
    }),
    "catalog and final-classification executable identity conflict",
  ]);
  tests.push([
    "identity conflict evidence-tier bypass",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "hades-ii-steam-release-test",
      );
      game.evidenceTier = "product_policy_survey";
      game.blockers = game.blockers.filter(
        (blocker) => blocker !== "catalog_final_executable_identity_conflict",
      );
    }),
    "catalog and final-classification executable identity conflict",
  ]);
  tests.push([
    "identity conflict runtime promotion",
    mutate((copy) => {
      const game = copy.games.find(
        (entry) => entry.gameId === "mount-and-blade-2-bannerlord-steam",
      );
      game.capability = "local_trial";
      game.runtimeMode = "invalid_conflicted_runtime";
      game.depthCaptureAvailable = true;
    }),
    "catalog/final executable identity conflict cannot expose a Depth runtime",
  ]);
  for (const [name, candidate, expected] of tests) {
    const errors = validateDepthCapabilities(
      candidate,
      shipped,
      finalClassifications,
      rightsGates,
    );
    if (!errors.some((error) => error.includes(expected))) {
      throw new Error(`self-test ${name} did not produce ${expected}: ${errors.join("; ")}`);
    }
  }

  const resolvedConflictClassifications = structuredClone(finalClassifications);
  resolvedConflictClassifications.records.find(
    (record) => String(record.app_id) === "261550",
  ).executable.sha256 = "5fc59a9da65474a707fd3e868a24c6f0a782e9364edd4d17a8f8813171237de0";
  const staleConflictErrors = validateDepthCapabilities(
    capabilities,
    shipped,
    resolvedConflictClassifications,
    rightsGates,
  );
  if (
    !staleConflictErrors.some((error) => error.includes("is stale because both identities match"))
  ) {
    throw new Error(
      `self-test stale identity blocker did not fail closed: ${staleConflictErrors.join("; ")}`,
    );
  }

  const wrongBasenameClassifications = structuredClone(finalClassifications);
  wrongBasenameClassifications.records.find(
    (record) => String(record.app_id) === "8870",
  ).executable.relative_install_path = "BioShock Infinite\\Binaries\\Win32\\Wrong.exe";
  const wrongBasenameErrors = validateDepthCapabilities(
    capabilities,
    shipped,
    wrongBasenameClassifications,
    rightsGates,
  );
  if (!wrongBasenameErrors.some((error) => error.includes("identity conflict"))) {
    throw new Error(
      `self-test final executable basename mismatch did not fail closed: ${wrongBasenameErrors.join("; ")}`,
    );
  }

  for (const malformedHash of ["", ` sha256:${"c".repeat(64)} `]) {
    const malformedHashClassifications = structuredClone(finalClassifications);
    malformedHashClassifications.records.find(
      (record) => String(record.app_id) === "8870",
    ).executable.sha256 = malformedHash;
    const malformedHashErrors = validateDepthCapabilities(
      capabilities,
      shipped,
      malformedHashClassifications,
      rightsGates,
    );
    if (!malformedHashErrors.some((error) => error.includes("SHA-256 is malformed"))) {
      throw new Error(
        `self-test malformed final executable hash did not fail closed: ${malformedHashErrors.join("; ")}`,
      );
    }
  }

  for (const unsafePath of [
    "",
    "BioShock Infinite\\Binaries\\Win32\\",
    "C:\\Games\\BioShockInfinite.exe",
    "BioShock Infinite\\.\\BioShockInfinite.exe",
    "..\\BioShockInfinite.exe",
    "BioShock Infinite\\Binaries\\Win32\\BioShockInfinite.exe:stream",
  ]) {
    const unsafePathClassifications = structuredClone(finalClassifications);
    unsafePathClassifications.records.find(
      (record) => String(record.app_id) === "8870",
    ).executable.relative_install_path = unsafePath;
    const unsafePathErrors = validateDepthCapabilities(
      capabilities,
      shipped,
      unsafePathClassifications,
      rightsGates,
    );
    if (!unsafePathErrors.some((error) => error.includes("safe relative Windows path"))) {
      throw new Error(
        `self-test unsafe final executable path did not fail closed: ${unsafePathErrors.join("; ")}`,
      );
    }
  }
  const runtimePendingAppId = "999999999";
  const rightsWithRuntimePending = structuredClone(rightsGates);
  rightsWithRuntimePending.records.push({
    app_id: runtimePendingAppId,
    game: "Synthetic Runtime Onboarding Fixture",
    catalog_game_ids: [],
    decision: "counsel_approved",
  });
  const capabilitiesWithRuntimePending = structuredClone(capabilities);
  capabilitiesWithRuntimePending.pendingOnboarding.push({
    gameId: "runtime-onboarding-fixture",
    displayName: "Runtime Onboarding Fixture",
    appId: runtimePendingAppId,
    targetBuildId: "1",
    steamStateFlags: 4,
    catalogAuthored: false,
    capability: "runtime_onboarding_pending",
    executableCandidate: "Fixture.exe",
    executableSha256: `sha256:${"a".repeat(64)}`,
    graphicsApi: "d3d11",
    runtimeEvidence: {
      schema: "dm-pending-runtime-evidence-v1",
      appId: runtimePendingAppId,
      targetBuildId: "1",
      executableBasename: "Fixture.exe",
      executableSha256: `sha256:${"a".repeat(64)}`,
      graphicsApi: "d3d11",
      launches: [
        {
          ordinal: 1,
          sourceReportSha256: `sha256:${"b".repeat(64)}`,
          sceneKind: "display_calibration",
          processIdentityMatched: true,
          graphicsApi: "d3d11",
          runtimeProtectionGate: "pass",
          window: {
            className: "SyntheticGameWindow",
            clientWidth: 1920,
            clientHeight: 1080,
          },
        },
        {
          ordinal: 2,
          sourceReportSha256: `sha256:${"c".repeat(64)}`,
          sceneKind: "main_menu",
          processIdentityMatched: true,
          graphicsApi: "d3d11",
          runtimeProtectionGate: "pass",
          window: {
            className: "SyntheticGameWindow",
            clientWidth: 1920,
            clientHeight: 1080,
          },
        },
      ],
    },
    blockers: [
      "rights_ledger_catalog_link_pending",
      "representative_single_player_runtime_evidence_missing",
      "d7_rgb_runtime_evidence_missing",
      "depth_profile_missing",
      "camera_binding_missing",
    ],
    evidence: ["tools/check-depth-capability-coverage.mjs"],
  });
  const runtimePendingFixtureErrors = validateDepthCapabilities(
    capabilitiesWithRuntimePending,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  if (runtimePendingFixtureErrors.length > 0) {
    throw new Error(
      `self-test valid runtime onboarding fixture failed validation: ${runtimePendingFixtureErrors.join("; ")}`,
    );
  }

  const runtimeFixture = (copy) =>
    copy.pendingOnboarding.find(
      (record) => String(record.appId) === runtimePendingAppId,
    );
  const expectRuntimeEvidenceError = (name, mutateFixture, expected) => {
    const candidate = structuredClone(capabilitiesWithRuntimePending);
    mutateFixture(runtimeFixture(candidate));
    const candidateErrors = validateDepthCapabilities(
      candidate,
      shipped,
      finalClassifications,
      rightsWithRuntimePending,
    );
    if (!candidateErrors.some((error) => error.includes(expected))) {
      throw new Error(
        `self-test ${name} did not fail closed for ${expected}: ${candidateErrors.join("; ")}`,
      );
    }
  };
  const mafiaFixture = (copy) =>
    copy.pendingOnboarding.find(
      (record) => record.gameId === MAFIA3_PENDING_GAME_ID,
    );
  const expectMafiaProfileError = (name, mutateFixture, expected) => {
    const candidate = structuredClone(capabilities);
    const fixture = mafiaFixture(candidate);
    if (!fixture?.localProfileEvidence) {
      throw new Error("self-test fixture requires Mafia III localProfileEvidence");
    }
    mutateFixture(fixture);
    const candidateErrors = validateDepthCapabilities(
      candidate,
      shipped,
      finalClassifications,
      rightsGates,
    );
    if (!candidateErrors.some((error) => error.includes(expected))) {
      throw new Error(
        `self-test ${name} did not fail closed for ${expected}: ${candidateErrors.join("; ")}`,
      );
    }
  };

  const oneLaunchRuntimePending = structuredClone(capabilitiesWithRuntimePending);
  const oneLaunchFixture = runtimeFixture(oneLaunchRuntimePending);
  oneLaunchFixture.runtimeEvidence.launches.pop();
  oneLaunchFixture.blockers.push("second_launch_window_identity_missing");
  const oneLaunchErrors = validateDepthCapabilities(
    oneLaunchRuntimePending,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  if (oneLaunchErrors.length > 0) {
    throw new Error(
      `self-test valid one-launch runtime evidence failed validation: ${oneLaunchErrors.join("; ")}`,
    );
  }

  const representativeRuntimePending = structuredClone(capabilitiesWithRuntimePending);
  const representativeFixture = runtimeFixture(representativeRuntimePending);
  representativeFixture.runtimeEvidence.launches[1].sceneKind =
    "representative_single_player";
  representativeFixture.blockers = representativeFixture.blockers.filter(
    (blocker) => blocker !== "representative_single_player_runtime_evidence_missing",
  );
  const representativeErrors = validateDepthCapabilities(
    representativeRuntimePending,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  if (representativeErrors.length > 0) {
    throw new Error(
      `self-test valid representative runtime evidence failed validation: ${representativeErrors.join("; ")}`,
    );
  }

  expectRuntimeEvidenceError(
    "missing runtime evidence",
    (fixture) => delete fixture.runtimeEvidence,
    "requires a structured runtimeEvidence summary",
  );
  expectRuntimeEvidenceError(
    "pending onboarding unsupported top-level field",
    (fixture) => {
      fixture.host = "synthetic-host";
    },
    "pendingOnboarding contains unsupported field host",
  );
  expectRuntimeEvidenceError(
    "pending onboarding duplicate blocker",
    (fixture) => {
      fixture.blockers.push(fixture.blockers[0]);
    },
    "blockers must be unique canonical strings",
  );
  expectRuntimeEvidenceError(
    "generic pending reuses Mafia profile evidence",
    (fixture) => {
      fixture.localProfileEvidence = structuredClone(
        mafiaFixture(capabilities).localProfileEvidence,
      );
    },
    "localProfileEvidence is reserved for the Mafia III schema discriminator",
  );
  for (const [field, value] of [
    ["appId", "111111111"],
    ["targetBuildId", "2"],
    ["executableBasename", "Wrong.exe"],
    ["executableSha256", `sha256:${"d".repeat(64)}`],
    ["graphicsApi", "d3d12"],
  ]) {
    expectRuntimeEvidenceError(
      `runtime evidence ${field} mismatch`,
      (fixture) => {
        fixture.runtimeEvidence[field] = value;
      },
      `${field} must match the pending identity`,
    );
  }
  for (const [name, mutateFixture, expected] of [
    [
      "runtime evidence unsupported top-level field",
      (fixture) => {
        fixture.runtimeEvidence.host = "synthetic-host";
      },
      "runtimeEvidence contains unsupported field host",
    ],
    [
      "runtime evidence wrong schema",
      (fixture) => {
        fixture.runtimeEvidence.schema = "wrong";
      },
      "runtimeEvidence schema is unsupported",
    ],
    [
      "runtime evidence empty launches",
      (fixture) => {
        fixture.runtimeEvidence.launches = [];
      },
      "launches must be a non-empty array",
    ],
    [
      "runtime evidence ordinal drift",
      (fixture) => {
        fixture.runtimeEvidence.launches[1].ordinal = 1;
      },
      "ordinal must be continuous from one",
    ],
    [
      "runtime evidence duplicate report hash",
      (fixture) => {
        fixture.runtimeEvidence.launches[1].sourceReportSha256 =
          fixture.runtimeEvidence.launches[0].sourceReportSha256;
      },
      "source report SHA-256 values must be unique",
    ],
    [
      "runtime evidence malformed report hash",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].sourceReportSha256 = "sha256:UNKNOWN";
      },
      "source report SHA-256 is malformed",
    ],
    [
      "runtime evidence unsupported scene",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].sceneKind = "unknown";
      },
      "sceneKind is unsupported",
    ],
    [
      "runtime evidence unmatched process",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].processIdentityMatched = false;
      },
      "process identity must be matched",
    ],
    [
      "runtime evidence API drift",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].graphicsApi = "d3d12";
      },
      "graphicsApi must match the pending identity",
    ],
    [
      "runtime evidence protection drift",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].runtimeProtectionGate = "blocked";
      },
      "runtime protection gate must pass",
    ],
    [
      "runtime evidence launch PII field",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].pid = 42;
      },
      "contains unsupported field pid",
    ],
    [
      "runtime evidence window PII field",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].window.title = "synthetic title";
      },
      "window contains unsupported field title",
    ],
    [
      "runtime evidence malformed window class",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].window.className = " bad\\n";
      },
      "window identity is malformed",
    ],
    [
      "runtime evidence missing window",
      (fixture) => {
        delete fixture.runtimeEvidence.launches[0].window;
      },
      "window must be an object",
    ],
    [
      "runtime evidence invalid window size",
      (fixture) => {
        fixture.runtimeEvidence.launches[0].window.clientWidth = 0;
      },
      "window identity is malformed",
    ],
    [
      "runtime evidence missing second-launch blocker",
      (fixture) => {
        fixture.runtimeEvidence.launches.pop();
      },
      "one runtime launch must retain the second-launch window blocker",
    ],
    [
      "runtime evidence stale second-launch blocker",
      (fixture) => {
        fixture.blockers.push("second_launch_window_identity_missing");
      },
      "second-launch window blocker is stale",
    ],
    [
      "runtime evidence inconsistent windows",
      (fixture) => {
        fixture.runtimeEvidence.launches[1].window.className = "OtherWindow";
      },
      "inconsistent window classes require an explicit blocker",
    ],
    [
      "runtime evidence missing representative blocker",
      (fixture) => {
        fixture.blockers = fixture.blockers.filter(
          (blocker) =>
            blocker !== "representative_single_player_runtime_evidence_missing",
        );
      },
      "non-representative runtimeEvidence must retain representative scene blocker",
    ],
    [
      "runtime evidence stale representative blocker",
      (fixture) => {
        fixture.runtimeEvidence.launches[1].sceneKind =
          "representative_single_player";
      },
      "representative scene blocker is stale",
    ],
  ]) {
    expectRuntimeEvidenceError(name, mutateFixture, expected);
  }

  for (const [name, mutateFixture, expected] of [
    [
      "missing local profile evidence",
      (fixture) => {
        delete fixture.localProfileEvidence;
      },
      "requires structured localProfileEvidence",
    ],
    [
      "local profile evidence unsupported field",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.host = "synthetic-host";
      },
      "localProfileEvidence.input_profile contains unsupported field host",
    ],
    [
      "local profile evidence schema drift",
      (fixture) => {
        fixture.localProfileEvidence.schema = "dm-mafia3-d6-local-profile-evidence/v2";
      },
      "localProfileEvidence.schema must be",
    ],
    [
      "local profile evidence validator hash drift",
      (fixture) => {
        fixture.localProfileEvidence.validator_sha256 = `sha256:${"c".repeat(64)}`;
      },
      "localProfileEvidence.validator_sha256 must be",
    ],
    [
      "local profile evidence app context drift",
      (fixture) => {
        fixture.localProfileEvidence.expected_app_id_context = "360431";
      },
      "localProfileEvidence.expected_app_id_context must be",
    ],
    [
      "local profile evidence launcher hash drift",
      (fixture) => {
        fixture.localProfileEvidence.launcher.sha256 = `sha256:${"d".repeat(64)}`;
      },
      "localProfileEvidence.launcher.sha256 must be",
    ],
    [
      "local profile evidence input hash drift",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.sha256 = `sha256:${"e".repeat(64)}`;
      },
      "localProfileEvidence.input_profile.sha256 must be",
    ],
    [
      "local profile evidence canonical controls hash drift",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.structure.canonical_control_set_sha256 =
          `sha256:${"f".repeat(64)}`;
      },
      "canonical_control_set_sha256 must be",
    ],
    [
      "local profile evidence logical control count drift",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.structure.controller_type_2_logical_controls =
          129;
      },
      "controller_type_2_logical_controls must be 130",
    ],
    [
      "local profile evidence core binding count drift",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.core_bindings.pop();
      },
      "localProfileEvidence.input_profile.core_bindings must contain exactly 31 entries",
    ],
    [
      "local profile evidence core binding value drift",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.core_bindings[0].element = 18;
      },
      "localProfileEvidence.input_profile.core_bindings[0].element must be 17",
    ],
    [
      "local profile evidence video hash drift",
      (fixture) => {
        fixture.localProfileEvidence.video_config.sha256 = `sha256:${"0".repeat(64)}`;
      },
      "localProfileEvidence.video_config.sha256 must be",
    ],
    [
      "local profile evidence resolution drift",
      (fixture) => {
        fixture.localProfileEvidence.video_config.fields.resolution_x = 2560;
      },
      "localProfileEvidence.video_config.fields.resolution_x must be 1920",
    ],
    [
      "local profile evidence fullscreen overclaim",
      (fixture) => {
        fixture.localProfileEvidence.video_config.fields.is_fullscreen = true;
      },
      "localProfileEvidence.video_config.fields.is_fullscreen must be false",
    ],
    [
      "local profile evidence HDR overclaim",
      (fixture) => {
        fixture.localProfileEvidence.video_config.fields.hdr_confirmed = true;
      },
      "localProfileEvidence.video_config.fields.hdr_confirmed must be false",
    ],
    [
      "local profile evidence default preset overclaim",
      (fixture) => {
        fixture.localProfileEvidence.input_profile.default_preset_confirmed = true;
      },
      "localProfileEvidence.input_profile.default_preset_confirmed must be false",
    ],
    [
      "local profile evidence RGB-ready overclaim",
      (fixture) => {
        fixture.localProfileEvidence.rgb_recording_ready = true;
      },
      "localProfileEvidence.rgb_recording_ready must be false",
    ],
    [
      "local profile evidence physical-mapping blocker missing",
      (fixture) => {
        fixture.localProfileEvidence.blockers =
          fixture.localProfileEvidence.blockers.filter(
            (blocker) =>
              blocker !== "physical_input_mapping_and_semantic_authoring_missing",
          );
      },
      "localProfileEvidence.blockers must contain exactly 7 entries",
    ],
  ]) {
    expectMafiaProfileError(name, mutateFixture, expected);
  }

  for (const requiredProfileBlocker of [
    "hdr_or_sdr_confirmation_missing",
    "input_profile_default_preset_confirmation_missing",
    "physical_input_mapping_and_semantic_authoring_missing",
  ]) {
    expectMafiaProfileError(
      `missing current local profile blocker ${requiredProfileBlocker}`,
      (fixture) => {
        fixture.blockers = fixture.blockers.filter(
          (blocker) => blocker !== requiredProfileBlocker,
        );
      },
      `localProfileEvidence requires current blocker ${requiredProfileBlocker}`,
    );
  }
  for (const staleProfileBlocker of [
    "display_mode_and_sdr_confirmation_missing",
    "local_input_mapping_evidence_missing",
  ]) {
    expectMafiaProfileError(
      `stale local profile blocker ${staleProfileBlocker}`,
      (fixture) => {
        fixture.blockers.push(staleProfileBlocker);
      },
      `localProfileEvidence makes blocker stale: ${staleProfileBlocker}`,
    );
  }
  expectMafiaProfileError(
    "local profile blocker diverges from pending blockers",
    (fixture) => {
      fixture.blockers = fixture.blockers.filter(
        (blocker) =>
          blocker !== "physical_input_mapping_and_semantic_authoring_missing",
      );
    },
    "localProfileEvidence and pending blockers must agree on physical_input_mapping_and_semantic_authoring_missing",
  );

  const rightsWithoutPending = structuredClone(rightsWithRuntimePending);
  rightsWithoutPending.records = rightsWithoutPending.records.filter(
    (record) => String(record.app_id) !== runtimePendingAppId,
  );
  const pendingRightsErrors = validateDepthCapabilities(
    capabilitiesWithRuntimePending,
    shipped,
    finalClassifications,
    rightsWithoutPending,
  );
  if (!pendingRightsErrors.some((error) => error.includes("pending AppID has no rights-ledger record"))) {
    throw new Error(
      `self-test missing pending rights record did not fail closed: ${pendingRightsErrors.join("; ")}`,
    );
  }

  const linkedPendingRights = structuredClone(rightsWithRuntimePending);
  linkedPendingRights.records.find(
    (record) => String(record.app_id) === runtimePendingAppId,
  ).catalog_game_ids = ["runtime-onboarding-fixture"];
  const linkedPendingErrors = validateDepthCapabilities(
    capabilitiesWithRuntimePending,
    shipped,
    finalClassifications,
    linkedPendingRights,
  );
  if (!linkedPendingErrors.some((error) => error.includes("must keep catalog_game_ids empty"))) {
    throw new Error(
      `self-test premature pending rights link did not fail closed: ${linkedPendingErrors.join("; ")}`,
    );
  }

  const unapprovedPendingRights = structuredClone(rightsWithRuntimePending);
  unapprovedPendingRights.records.find(
    (record) => String(record.app_id) === runtimePendingAppId,
  ).decision = "requested";
  const unapprovedPendingErrors = validateDepthCapabilities(
    capabilitiesWithRuntimePending,
    shipped,
    finalClassifications,
    unapprovedPendingRights,
  );
  if (!unapprovedPendingErrors.some((error) => error.includes("is not approved"))) {
    throw new Error(
      `self-test unapproved pending rights record did not fail closed: ${unapprovedPendingErrors.join("; ")}`,
    );
  }

  const incompleteRuntimePending = structuredClone(capabilitiesWithRuntimePending);
  const runtimePending = incompleteRuntimePending.pendingOnboarding.find(
    (record) => String(record.appId) === runtimePendingAppId,
  );
  runtimePending.steamStateFlags = 18;
  runtimePending.blockers.push("steam_install_incomplete");
  runtimePending.executableSha256 = "";
  const incompleteRuntimeErrors = validateDepthCapabilities(
    incompleteRuntimePending,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  for (const expected of [
    "requires complete Steam state",
    "contradicts steam_install_incomplete",
    "must pin lowercase executable SHA-256",
  ]) {
    if (!incompleteRuntimeErrors.some((error) => error.includes(expected))) {
      throw new Error(
        `self-test incomplete runtime onboarding did not fail closed for ${expected}: ${incompleteRuntimeErrors.join("; ")}`,
      );
    }
  }

  const duplicatePendingAppId = structuredClone(capabilitiesWithRuntimePending);
  duplicatePendingAppId.pendingOnboarding.push({
    ...structuredClone(runtimePending),
    gameId: "duplicate-pending-app-id-fixture",
  });
  const duplicatePendingAppIdErrors = validateDepthCapabilities(
    duplicatePendingAppId,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  if (!duplicatePendingAppIdErrors.some((error) => error.includes("duplicate pending onboarding appId"))) {
    throw new Error(
      `self-test duplicate pending AppID did not fail closed: ${duplicatePendingAppIdErrors.join("; ")}`,
    );
  }

  for (const unsafeExecutableCandidate of [
    "",
    " Fixture.exe ",
    "C:\\Games\\Fixture.exe",
    "bin\\Fixture.exe",
    "Fixture.dll",
  ]) {
    const unsafePendingExecutable = structuredClone(capabilitiesWithRuntimePending);
    unsafePendingExecutable.pendingOnboarding.find(
      (record) => String(record.appId) === runtimePendingAppId,
    ).executableCandidate = unsafeExecutableCandidate;
    const unsafePendingExecutableErrors = validateDepthCapabilities(
      unsafePendingExecutable,
      shipped,
      finalClassifications,
      rightsWithRuntimePending,
    );
    if (!unsafePendingExecutableErrors.some((error) => error.includes("one safe executable basename"))) {
      throw new Error(
        `self-test unsafe pending executable did not fail closed for ${JSON.stringify(unsafeExecutableCandidate)}: ${unsafePendingExecutableErrors.join("; ")}`,
      );
    }
  }

  for (const requiredBlocker of [
    "d7_rgb_runtime_evidence_missing",
    "depth_profile_missing",
    "camera_binding_missing",
  ]) {
    const missingRuntimeBlocker = structuredClone(capabilitiesWithRuntimePending);
    const pending = missingRuntimeBlocker.pendingOnboarding.find(
      (record) => String(record.appId) === runtimePendingAppId,
    );
    pending.blockers = pending.blockers.filter((blocker) => blocker !== requiredBlocker);
    const missingRuntimeBlockerErrors = validateDepthCapabilities(
      missingRuntimeBlocker,
      shipped,
      finalClassifications,
      rightsWithRuntimePending,
    );
    if (!missingRuntimeBlockerErrors.some((error) => error.includes(`must retain ${requiredBlocker}`))) {
      throw new Error(
        `self-test missing runtime blocker did not fail closed for ${requiredBlocker}: ${missingRuntimeBlockerErrors.join("; ")}`,
      );
    }
  }

  for (const staleIdentityBlocker of [
    "shipping_process_identity_missing",
    "graphics_api_runtime_unverified",
  ]) {
    const staleRuntimeBlocker = structuredClone(capabilitiesWithRuntimePending);
    staleRuntimeBlocker.pendingOnboarding.find(
      (record) => String(record.appId) === runtimePendingAppId,
    ).blockers.push(staleIdentityBlocker);
    const staleRuntimeBlockerErrors = validateDepthCapabilities(
      staleRuntimeBlocker,
      shipped,
      finalClassifications,
      rightsWithRuntimePending,
    );
    if (!staleRuntimeBlockerErrors.some((error) => error.includes(`retains stale blocker ${staleIdentityBlocker}`))) {
      throw new Error(
        `self-test stale runtime onboarding blocker did not fail closed for ${staleIdentityBlocker}: ${staleRuntimeBlockerErrors.join("; ")}`,
      );
    }
  }

  for (const runtimeOnlyField of ["runtimeMode", "depthCaptureAvailable", "recordingReady"]) {
    const authorizedPending = structuredClone(capabilitiesWithRuntimePending);
    authorizedPending.pendingOnboarding.find(
      (record) => String(record.appId) === runtimePendingAppId,
    )[runtimeOnlyField] = true;
    const authorizedPendingErrors = validateDepthCapabilities(
      authorizedPending,
      shipped,
      finalClassifications,
      rightsWithRuntimePending,
    );
    if (!authorizedPendingErrors.some((error) => error.includes(`runtime field ${runtimeOnlyField}`))) {
      throw new Error(
        `self-test pending runtime authorization did not fail closed for ${runtimeOnlyField}: ${authorizedPendingErrors.join("; ")}`,
      );
    }
  }

  const authorizedInstallBlocked = structuredClone(capabilitiesWithRuntimePending);
  const installBlocked = authorizedInstallBlocked.pendingOnboarding.find(
    (record) => String(record.appId) === runtimePendingAppId,
  );
  installBlocked.capability = "install_blocked";
  installBlocked.steamStateFlags = 18;
  installBlocked.blockers.push("steam_install_incomplete");
  installBlocked.executableCandidate = null;
  delete installBlocked.executableSha256;
  delete installBlocked.graphicsApi;
  delete installBlocked.runtimeEvidence;
  delete installBlocked.localProfileEvidence;
  installBlocked.recordingReady = true;
  installBlocked.executableCandidate = "C:\\Games\\Mafia3.exe";
  installBlocked.executableSha256 = `sha256:${"a".repeat(64)}`;
  installBlocked.graphicsApi = "d3d11";
  installBlocked.runtimeEvidence = {};
  installBlocked.localProfileEvidence = {};
  const authorizedInstallBlockedErrors = validateDepthCapabilities(
    authorizedInstallBlocked,
    shipped,
    finalClassifications,
    rightsWithRuntimePending,
  );
  for (const expected of [
    "runtime field recordingReady",
    "must keep executableCandidate null",
    "unavailable runtime field executableSha256",
    "unavailable runtime field graphicsApi",
    "unavailable runtime field runtimeEvidence",
    "unavailable runtime field localProfileEvidence",
  ]) {
    if (!authorizedInstallBlockedErrors.some((error) => error.includes(expected))) {
      throw new Error(
        `self-test install-blocked runtime authorization did not fail closed for ${expected}: ${authorizedInstallBlockedErrors.join("; ")}`,
      );
    }
  }

  const shippedWithPending = structuredClone(shipped);
  shippedWithPending.games.push({
    gameId: "runtime-onboarding-fixture",
    displayName: "Pending fixture",
    catalogDir: "gta5-legacy-pc",
    admission: "onboarding",
  });
  const shippedPendingErrors = validateDepthCapabilities(
    capabilitiesWithRuntimePending,
    shippedWithPending,
    finalClassifications,
    rightsWithRuntimePending,
  );
  if (!shippedPendingErrors.some((error) => error.includes("cannot be pending and shipped"))) {
    throw new Error(
      `self-test pending/shipped overlap did not fail closed: ${shippedPendingErrors.join("; ")}`,
    );
  }
}

const capabilities = loadJson(CAPABILITIES_PATH);
const shipped = loadJson(SHIPPED_PATH);
const finalClassifications = loadJson(FINAL_CLASSIFICATIONS_PATH);
const rightsGates = loadJson(RIGHTS_GATES_PATH);
const performanceCertifications = loadJson(PERFORMANCE_CERTIFICATIONS_PATH);
if (process.argv.includes("--self-test")) {
  selfTestEvidencePaths();
  selfTest(
    capabilities,
    shipped,
    finalClassifications,
    rightsGates,
    performanceCertifications,
  );
}
const errors = validateDepthCapabilities(
  capabilities,
  shipped,
  finalClassifications,
  rightsGates,
  performanceCertifications,
);
if (errors.length > 0) {
  console.error(`Depth capability coverage failed:\n- ${errors.join("\n- ")}`);
  process.exit(1);
}
const counts = Object.fromEntries(
  [...ALLOWED_CAPABILITIES].map((capability) => [
    capability,
    capabilities.games.filter((game) => game.capability === capability).length,
  ]),
);
console.log(
  `PASS: ${capabilities.games.length} shipped games covered; ` +
    Object.entries(counts).map(([name, count]) => `${name}=${count}`).join(", ") +
    `; recording_ready=${capabilities.games.filter((game) => game.recordingReady).length}` +
    `; pending_onboarding=${capabilities.pendingOnboarding.length}` +
    `; final_ledger_records=${capabilities.finalLedgerAudit.ledgerTotalRecords}` +
    `; mapped_final_games=${capabilities.finalLedgerAudit.mappedShippedGames}` +
    `; hard_terminal_authority=${capabilities.finalLedgerAudit.hardTerminalMappedGames}` +
    `; provisional_authority=${capabilities.finalLedgerAudit.provisionalMappedGames}`,
);
