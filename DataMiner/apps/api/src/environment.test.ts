import { describe, expect, it } from "vitest";

import { classificationSchemaFor, createSessionSchemaFor } from "./contracts.js";
import {
  environmentProfile,
  environments,
  primaryFamily,
  sessionArtifactObjectKey,
  storagePolicyForSessionSchema,
  type AdmissionFamily,
} from "./environment.js";
import { clipArtifactObjectKey } from "./repository.js";

describe("environment profiles", () => {
  it("keeps staging and production primary families on disjoint object namespaces", () => {
    const staging = primaryFamily(environmentProfile("staging"));
    const production = primaryFamily(environmentProfile("production"));

    expect(staging.storageNamespace).not.toBe(production.storageNamespace);
    expect(staging.objectClass).not.toBe(production.objectClass);
    // A prefix valid in one environment must be invalid in the other, so a
    // misconfigured deployment cannot write into the other's key space.
    expect(production.prefixPattern.test(staging.defaultPrefix!)).toBe(false);
    expect(staging.prefixPattern.test("raw/v1/dataminer-api-production")).toBe(false);
  });

  it("gives production no default prefix", () => {
    for (const family of environmentProfile("production").families) {
      expect(family.defaultPrefix).toBeNull();
    }
  });

  it("keeps staging non-trainable at the source whatever else is true", () => {
    const staging = environmentProfile("staging");
    // Exactly one family: the consented lane must not exist there, so staging
    // cannot be used to rehearse the trainable path with fake consent rows.
    expect(staging.families).toHaveLength(1);
    expect(primaryFamily(staging).bestClassification).toBe("non_trainable");
    expect(primaryFamily(staging).admission.nonTrainable).toBe(true);
    expect(primaryFamily(staging).admission.productionCertified).toBe(false);
  });

  it("never declares a session trainable, in any defined family", () => {
    // `isTrainable` is decided by clip_trainability, never by an admission
    // envelope. A candidate-capable lane raises the ceiling to
    // training_candidate; it does not assert the data is trainable.
    //
    // 断言写成"不得为 true"而不是"必须为 false"：中性摄取契约**整个不携带**这个键
    // （issue #855 §B），缺席与显式 false 都算通过，只有显式 true 不行。
    for (const environment of environments) {
      for (const family of environmentProfile(environment).families) {
        expect(family.admission.isTrainable === true).toBe(false);
        expect(family.admission.productionCertified === true).toBe(false);
      }
    }
  });

  it("keeps one dev vocabulary in every environment", () => {
    // Production's only family admits the same dm-local-dev uploads as staging.
    expect(primaryFamily(environmentProfile("staging")).catalogMode).toBe("dev_non_trainable");
    expect(primaryFamily(environmentProfile("production")).catalogMode).toBe("dev_non_trainable");
    expect(primaryFamily(environmentProfile("production")).objectClass).toBe("raw-non-trainable");
    expect(primaryFamily(environmentProfile("production")).bestClassification).toBe("non_trainable");
  });

  // consented first-party 车道随 docs/57 §3.3 退役。这里从"它长什么样"翻面成
  // "它确实不在了"——一条准入家族被摘掉，最容易的失败方式是没人再提它，于是
  // 半年后有人照着旧文档把 R2_PREFIX_FIRST_PARTY 配回去、数据写进一个没有家族
  // 收的前缀。留一条断言把这件事钉住。
  it("no longer defines the first-party lane in any environment", () => {
    for (const environment of ["staging", "production"] as const) {
      expect(
        environmentProfile(environment).families.some(
          (family) => family.sourceFamily === "dm-first-party-release",
        ),
      ).toBe(false);
    }
    expect(primaryFamily(environmentProfile("production")).bestClassification).toBe(
      "non_trainable",
    );
  });

  // certified 车道随 docs/100 退役。与 first-party 同一条纪律：一条准入家族被摘
  // 掉，最容易的失败方式是没人再提它，于是有人照旧文档把 R2_PREFIX_CERTIFIED
  // 配回去、数据写进一个没有家族收的前缀。钉住它确实不在了。
  it("no longer defines the certified lane in any environment", () => {
    for (const environment of ["staging", "production"] as const) {
      const families = environmentProfile(environment).families;
      expect(families.some((family) => family.sourceFamily === "dm-certified")).toBe(false);
      expect(families.some((family) => family.catalogMode === "certified")).toBe(false);
      // 没有任何家族能自己走到**可训练**。逐游戏 grant 那道闸随 docs/108 D1 摘除后，
      // 这条是"准入面不发放可训练性"的唯一结构性断言——授权由 consent 如实记录，
      // 可训练性仍只能由发布侧人工放行开启（AGENTS.md 硬约束）。
      //
      // `training_candidate` 不是这条断言的例外：它表示"有资格等待裁决"，
      // `clip_trainability` 在全部 blocker 清零前照旧判 false（issue #855 §C）。
      // 所以这里钉的是**词表**：只有这两个值，不得出现第三个。
      for (const family of families) {
        expect(["non_trainable", "training_candidate"]).toContain(family.bestClassification);
      }
    }
  });

  // 家族清单是显式裁决出来的，不该是一次合并的副作用。多一条、少一条都要在这里
  // 被看见。staging 恒一条；production 两条 = 已准入的 legacy 车道 + issue #855
  // 定义但尚未准入的中性摄取车道。
  //
  // "定义"与"准入"是两件事：`loadConfig` 今天只把 primary family 放进
  // `admittedFamilies`（见 config.test.ts 的 `admittedFamilies` 断言），所以这里多
  // 出来的一条不会让任何请求走到新车道。
  it("defines exactly the deliberated families per environment", () => {
    expect(environmentProfile("staging").families.map((family) => family.sourceFamily)).toEqual([
      "dm-local-dev",
    ]);
    expect(environmentProfile("production").families.map((family) => family.sourceFamily)).toEqual([
      "dm-local-dev",
      "dm-session-ingest",
    ]);
  });
});

// ── 中性摄取契约（issue #855 §B）──────────────────────────────────────────────
//
// PR 1 只定义它。这些断言把定义钉死，好让 PR 3 的实现有个不会漂的靶子——也让
// "去环境化"这件事变成可执行的检查而不是一句口号。
describe("the neutral ingest contract", () => {
  const neutral = environmentProfile("production").families.find(
    (family) => family.sessionSchemaVersion === "dm-session-ingest-2",
  )!;

  it("names nothing after an environment, a channel, or a trainability verdict", () => {
    const vocabulary = [
      neutral.sourceFamily,
      neutral.sessionSchemaVersion,
      neutral.artifactSchemaVersion,
      neutral.objectClass,
      neutral.preflightResult,
      neutral.prefixHint,
      neutral.admission.environment === "production" ? "" : neutral.admission.environment,
    ].join(" ");
    for (const retiredWord of ["dev", "certified", "staging", "non_trainable", "non-trainable"]) {
      expect(vocabulary).not.toContain(retiredWord);
    }
  });

  it("pins the raw/v2 layout the issue specifies", () => {
    expect(neutral.objectClass).toBe("dataminer-raw");
    expect(neutral.prefixHint).toBe("raw/v2/<deployment>");
    expect(neutral.prefixPattern.test("raw/v2/dataminer-api-production")).toBe(true);
    // 前缀必须是排他的部署级前缀，不能是车道根本身，也不能越进 v1 的键空间。
    expect(neutral.prefixPattern.test("raw/v2")).toBe(false);
    expect(neutral.prefixPattern.test("raw/v1/non-trainable/dataminer-api-production")).toBe(false);
    expect(primaryFamily(environmentProfile("production")).prefixPattern.test("raw/v2/x-y-z")).toBe(
      false,
    );
    // 没有默认前缀：新车道要么被部署显式命名，要么整条不出现。
    expect(neutral.defaultPrefix).toBeNull();
    // clip 的 key 生成器本来就已经是 `<prefix>/sessions/<id>/runs/...`，所以
    // `raw/v2/<deployment>` + 既有布局逐字就是 issue 写的那一条。
    expect(
      clipArtifactObjectKey({
        objectPrefix: "raw/v2/dataminer-api-production",
        sessionId: "s-1",
        runId: "r-1",
        clipId: "c-1",
        kind: "video",
        sha256: `sha256:${"a".repeat(64)}`,
      }),
    ).toBe(
      `raw/v2/dataminer-api-production/sessions/s-1/runs/r-1/clips/c-1/video-${"a".repeat(64)}.mp4`,
    );
  });

  it("carries no catalog mode and no duplicated training booleans", () => {
    expect(neutral.catalogMode).toBeNull();
    expect(neutral.admission).not.toHaveProperty("nonTrainable");
    expect(neutral.admission).not.toHaveProperty("isTrainable");
    expect(neutral.admission).not.toHaveProperty("productionCertified");
    expect(neutral.retiredClassificationKeys).toEqual([
      "nonTrainable",
      "isTrainable",
      "productionCertified",
      "catalogMode",
    ]);
    // legacy 车道**不受影响**：那些键正是它的契约。
    expect(primaryFamily(environmentProfile("production")).retiredClassificationKeys).toEqual([]);
  });

  it("caps at training_candidate, which is not a pass", () => {
    expect(neutral.bestClassification).toBe("training_candidate");
    // legacy 车道永久 non_trainable，一个字节没变（issue #855 §D）。
    expect(primaryFamily(environmentProfile("production")).bestClassification).toBe("non_trainable");
    expect(primaryFamily(environmentProfile("staging")).bestClassification).toBe("non_trainable");
  });

  it("has exactly one session schema version, because that is the routing key", () => {
    // legacy 车道用两代 session schema 区分有没有音频；§B 规定 v2 只有一个值作为
    // 唯一路由键，所以音频在 v2 上是 artifact 集合的事实，不是 schema 世代。
    expect(neutral.audio).toBeUndefined();
    expect(neutral.audioDeclaration).toBe("per_clip");
    expect(primaryFamily(environmentProfile("production")).audioDeclaration).toBe("session_schema");
  });

  it("builds a request schema that carries the identity and drops the lane words", () => {
    // PR 3 把这条车道的请求契约落下来了：Catalog 身份一个字段没少（id / version /
    // contentHash），少掉的只有 `mode` 与三个训练布尔。
    const schema = createSessionSchemaFor(neutral);
    const shape = (schema as unknown as { _def: { shape: Record<string, unknown> } })._def.shape;
    expect(Object.keys(shape)).toContain("catalog");
    const classification = classificationSchemaFor(neutral);
    expect(
      classification.safeParse({
        sourceFamily: "dm-session-ingest",
        contractVersion: "dm-1",
        artifactSchemaVersion: "dm-raw-recording-1",
        environment: "production",
        storageNamespace: "raw",
      }).success,
    ).toBe(true);
    for (const retired of neutral.retiredClassificationKeys) {
      expect(
        classification.safeParse({
          sourceFamily: "dm-session-ingest",
          contractVersion: "dm-1",
          artifactSchemaVersion: "dm-raw-recording-1",
          environment: "production",
          storageNamespace: "raw",
          [retired]: false,
        }).success,
      ).toBe(false);
    }
  });
});

// ── v1/v2 并存期的唯一路由键（issue #855 §B）─────────────────────────────────
describe("storage routing by persisted sessions.schema_version", () => {
  const production = environmentProfile("production");

  it("keeps every legacy session on the legacy lane", () => {
    for (const version of ["dm-local-dev-session-2", "dm-local-dev-session-3"]) {
      const family = storagePolicyForSessionSchema(production, version);
      expect(family.objectClass).toBe("raw-non-trainable");
      expect(family.prefixHint).toBe("raw/v1/non-trainable/<deployment>");
      expect(family.bestClassification).toBe("non_trainable");
    }
  });

  it("routes the neutral contract to raw/v2", () => {
    const family = storagePolicyForSessionSchema(production, "dm-session-ingest-2");
    expect(family.objectClass).toBe("dataminer-raw");
    expect(family.prefixHint).toBe("raw/v2/<deployment>");
  });

  it("refuses an unknown version instead of falling back to a lane", () => {
    // 「未知版本明确拒绝」。静默落到 families[0] 正是让新数据写进旧键空间的那条路。
    for (const unknown of ["dm-certified-session-3", "dm-local-dev-session-1", ""]) {
      expect(() => storagePolicyForSessionSchema(production, unknown)).toThrow(
        /unsupported session schema version/,
      );
    }
  });
});

describe("session artifact object keys", () => {
  it("keeps the legacy flat shape byte-for-byte", () => {
    expect(sessionArtifactObjectKey("raw/v1/non-trainable/dm", "s-1", "session.json")).toBe(
      "raw/v1/non-trainable/dm/s-1/session/session.json",
    );
    expect(
      sessionArtifactObjectKey("raw/v1/non-trainable/dm", "s-1", "session.json", "legacy_flat"),
    ).toBe("raw/v1/non-trainable/dm/s-1/session/session.json");
  });

  it("puts every raw/v2 object under the same session root", () => {
    expect(
      sessionArtifactObjectKey("raw/v2/dm", "s-1", "catalog.json", "session_scoped"),
    ).toBe("raw/v2/dm/sessions/s-1/session/catalog.json");
  });
});

const staging = primaryFamily(environmentProfile("staging"));
const production = primaryFamily(environmentProfile("production"));
const digest = `sha256:${"a".repeat(64)}`;

/** A session declaration valid for `family`, built from its own admission envelope. */
function sessionFor(family: AdmissionFamily) {
  const ref = (id: string) => ({ id, version: "1", contentHash: digest });
  return {
    localSessionId: "dev-1784791023860-0000",
    sessionSchemaVersion: family.sessionSchemaVersion,
    startedAt: "2026-07-23T00:00:00.000Z",
    localState: "completed",
    classification: { ...family.admission },
    clientVersion: "0.1.0",
    catalog: {
      id: "dataminer-some-catalog",
      version: "1",
      mode: family.catalogMode as string,
      contentHash: digest,
    },
    game: { id: "some-game", version: "1", contentHash: digest, executableSha256: digest },
    deviceProfile: ref("some-device"),
    recordingProfile: {
      ...ref("some-recording"),
      width: 1920,
      height: 1080,
      fpsNum: 60,
      fpsDen: 1,
      captureEngine: "native_wgc_window_v1",
    },
    mapping: ref("some-mapping"),
  };
}

describe("admission envelopes do not cross", () => {
  it("refuses a production session on the staging profile and vice versa", () => {
    const productionSession = sessionFor(production);
    expect(createSessionSchemaFor(production).safeParse(productionSession).success).toBe(true);
    expect(createSessionSchemaFor(staging).safeParse(productionSession).success).toBe(false);

    const stagingSession = sessionFor(staging);
    expect(createSessionSchemaFor(staging).safeParse(stagingSession).success).toBe(true);
    expect(createSessionSchemaFor(production).safeParse(stagingSession).success).toBe(false);
  });

  /// 退役的 certified 信封（docs/100）必须被逐键比对挡下，而不是因为「多一个未知
  /// 家族名」而被当成形状错误糊弄过去——一个还没升级的旧客户端发来它，答复必须
  /// 明确，否则那些字节会静静地进不去也报不清。
  it("refuses the retired certified admission envelope everywhere", () => {
    const retired = {
      ...sessionFor(production),
      sessionSchemaVersion: "dm-certified-session-2",
      classification: {
        sourceFamily: "dm-certified",
        contractVersion: "dm-1",
        artifactSchemaVersion: "dm-certified-recording-1",
        environment: "production",
        storageNamespace: "raw",
        nonTrainable: false,
        isTrainable: false,
        productionCertified: true,
      },
    };
    expect(createSessionSchemaFor(production).safeParse(retired).success).toBe(false);
    expect(createSessionSchemaFor(staging).safeParse(retired).success).toBe(false);
  });

  it("refuses a retired certified catalog mode", () => {
    const base = sessionFor(production);
    const wrongCatalog = { ...base, catalog: { ...base.catalog, mode: "certified" } };
    expect(createSessionSchemaFor(production).safeParse(wrongCatalog).success).toBe(false);
  });
});
