/**
 * What an environment *is*, in one place.
 *
 * Before this existed, "staging" was spelled out as `z.literal` values scattered
 * across the admission contract, the classification guard, the R2 object
 * metadata check, and the SQL. Adding production meant finding every one of them
 * and hoping none was missed. Now each environment is a row in this table and
 * every one of those sites reads from it.
 *
 * v2 (docs/49 E7): an environment no longer admits exactly one artifact family.
 * It admits a *list* of families, each with its own classification ceiling,
 * catalog mode, storage prefix, and object class. `families[0]` is the primary
 * family — the one `R2_PREFIX` names and the one every deployment must serve.
 * Additional families are admitted only when the deployment explicitly
 * configures their prefix (see config.ts), so the trainable lane cannot appear
 * by default.
 *
 * Admitting a family does **not** make its data trainable. Trainability is
 * decided by `clip_trainability`, which still requires an active consent, an
 * active grant, an approved catalog, and a QA pass. A family's
 * `bestClassification` only sets the ceiling `createSession` may reach when
 * every governance fact is present.
 */
// `certified` 曾是第三个部署环境，随后变成 production 上的第二条准入家族，
// 最终随平行授权车道整条退役（docs/100）。今天 production 只有一条**已准入**的家族；
// 中性摄取家族已按 issue #855 §B 定义在此文件里，只在部署显式命名 `R2_PREFIX_INGEST_V2`
// 时被 `loadConfig` 准入。
export const environments = ["staging", "production"] as const;
export type Environment = (typeof environments)[number];

export type AdmissionFamily = {
  /** The lane name the client declares as `classification.sourceFamily`. */
  sourceFamily: string;
  contractVersion: "dm-1";
  artifactSchemaVersion: string;
  sessionSchemaVersion: string;
  /** Optional process-audio generation accepted alongside the legacy family. */
  audio?: {
    artifactSchemaVersion: string;
    sessionSchemaVersion: string;
  };

  /**
   * 「这一场采集有没有音频」由谁回答。
   *
   * - `session_schema`：legacy v1——由 session schema 世代回答（`-2` 无音频、`-3` 有），
   *   `audio` 给出音频世代的一对版本号；
   * - `per_clip`：v2——`dm-session-ingest-2` 是**唯一**的 session schema（§B 的唯一
   *   路由键），所以"有没有音频"只能是 clip 的 artifact 事实（PR 1 §7.2）。
   *
   * 去掉的是"用 schema 世代表达音频"这件事，**不是那道闸**：录制档案有没有配置
   * 音频仍由服务端的 recording-profile 快照裁决，clip 的申报必须与它一致
   * （`assertRecordingProfileAudioFacts` / `putClip` 的 `clip_audio_not_in_profile`）。
   */
  audioDeclaration: "session_schema" | "per_clip";

  /** Object layout. */
  storageNamespace: string;
  /** `x-amz-meta-dataminer_class` stamped at multipart creation and required on every stored object. */
  objectClass: string;
  prefixPattern: RegExp;
  prefixHint: string;
  /** The primary family may have a safe default; every other family must be named explicitly. */
  defaultPrefix: string | null;

  /**
   * Catalog and preflight vocabulary.
   *
   * `catalogMode` 是 legacy v1 车道的遗留词（`dev_non_trainable`）。中性摄取契约
   * 不再携带它（issue #855 §A.4、§B），因此新家族为 `null`；v1/v2 的存储路由改由
   * 持久化的 `sessions.schema_version` 唯一决定，见 `storagePolicyForSessionSchema`。
   */
  catalogMode: string | null;
  preflightResult: string;

  /**
   * The classification a session may reach when every governance fact is present.
   *
   * `production_candidate` 于 issue #855 更名 `training_candidate`：production 是
   * 部署环境，candidate 是训练资格状态，两者不该串台。数据库枚举同名改名
   * （migration 0036），历史零行，纯改名。
   */
  bestClassification: "non_trainable" | "training_candidate";

  /**
   * Declared admission envelope. A session whose classification block differs
   * from this in any key is refused before structural validation.
   *
   * 三个训练布尔在中性契约里**整体缺席**（issue #855 §B：不重复携带互相可能打架的
   * 字段），所以是可选的。可选**不等于**可以随便带：`retiredClassificationKeys`
   * 列出的键在中性车道上出现即拒绝（app.ts 的 `enforceDeclaredClassification`，
   * 403 policy_blocked；契约层的 `.strict()` 是第二道）。
   */
  admission: {
    sourceFamily: string;
    contractVersion: string;
    artifactSchemaVersion: string;
    environment: string;
    storageNamespace: string;
    nonTrainable?: boolean;
    isTrainable?: boolean;
    productionCertified?: boolean;
  };

  /**
   * 这条车道上**必须缺席**的 classification 键。legacy 车道为空（那些键正是它的
   * 契约）；中性车道列出全部退役词，让"新契约不再携带它们"成为可执行的断言而不是
   * 一句注释。
   */
  retiredClassificationKeys: readonly string[];

  /**
   * 会话级快照对象的 key 形状。
   *
   * `legacy_flat` 是 v1 的历史形状 `<prefix>/<session-id>/session/<file>`——它和同
   * 一车道 clip 的 `<prefix>/sessions/<id>/runs/...` 本来就不一致，是历史遗留而非
   * 设计。`session_scoped` 是 raw/v2 的统一形状，让整个 session 的所有对象都落在
   * `raw/v2/<deployment>/sessions/<session-id>/` 之下（issue #855 §B）。
   */
  sessionArtifactKeyStyle: "legacy_flat" | "session_scoped";
};

export type EnvironmentProfile = {
  environment: Environment;
  /** families[0] is the primary family; its prefix comes from R2_PREFIX. */
  families: AdmissionFamily[];
};

const stagingDev: AdmissionFamily = {
  sourceFamily: "dm-local-dev",
  contractVersion: "dm-1",
  artifactSchemaVersion: "dm-local-dev-recording-1",
  sessionSchemaVersion: "dm-local-dev-session-2",
  audio: {
    artifactSchemaVersion: "dm-local-dev-recording-2",
    sessionSchemaVersion: "dm-local-dev-session-3",
  },
  audioDeclaration: "session_schema",
  storageNamespace: "raw-staging",
  objectClass: "raw-staging-non-trainable",
  prefixPattern: /^raw-staging\/v1\/non-trainable\/[a-z0-9][a-z0-9-]{2,63}\/*$/,
  prefixHint: "raw-staging/v1/non-trainable/<deployment>",
  defaultPrefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
  catalogMode: "dev_non_trainable",
  preflightResult: "dev_non_trainable_pass",
  // Staging data is non-trainable at the source no matter what else is true.
  bestClassification: "non_trainable",
  admission: {
    sourceFamily: "dm-local-dev",
    contractVersion: "dm-1",
    artifactSchemaVersion: "dm-local-dev-recording-1",
    environment: "staging",
    storageNamespace: "raw-staging",
    nonTrainable: true,
    isTrainable: false,
    productionCertified: false,
  },
  retiredClassificationKeys: [],
  sessionArtifactKeyStyle: "legacy_flat",
};

// Production's primary family is what #75 shipped: the client RAW uploader
// declaring the same dm-local-dev family as staging, landing under
// raw/v1/non-trainable/. It stays non-trainable at the source: a dm-local-dev
// RAW upload can never become trainable no matter which governance rows exist.
const productionDev: AdmissionFamily = {
  sourceFamily: "dm-local-dev",
  contractVersion: "dm-1",
  artifactSchemaVersion: "dm-local-dev-recording-1",
  sessionSchemaVersion: "dm-local-dev-session-2",
  audio: {
    artifactSchemaVersion: "dm-local-dev-recording-2",
    sessionSchemaVersion: "dm-local-dev-session-3",
  },
  audioDeclaration: "session_schema",
  storageNamespace: "raw",
  objectClass: "raw-non-trainable",
  prefixPattern: /^raw\/v1\/non-trainable\/[a-z0-9][a-z0-9-]{2,63}\/*$/,
  prefixHint: "raw/v1/non-trainable/<deployment>",
  // No default: a production deployment names its own prefix or does not start.
  defaultPrefix: null,
  catalogMode: "dev_non_trainable",
  preflightResult: "dev_non_trainable_pass",
  bestClassification: "non_trainable",
  admission: {
    sourceFamily: "dm-local-dev",
    contractVersion: "dm-1",
    artifactSchemaVersion: "dm-local-dev-recording-1",
    environment: "production",
    storageNamespace: "raw",
    nonTrainable: true,
    isTrainable: false,
    productionCertified: false,
  },
  retiredClassificationKeys: [],
  sessionArtifactKeyStyle: "legacy_flat",
};

/**
 * 中性摄取契约（issue #855 §B）。PR 1 定义词表，PR 3 交付摄取与路由。
 *
 * **它仍然默认不出现**：`loadConfig` 只在部署显式命名 `R2_PREFIX_INGEST_V2` 时才把
 * 它放进 `admittedFamilies`（config.ts）。没有那一项，没有任何请求走得到这条车道，
 * 也没有任何 session 会被写成 `dm-session-ingest-2`——"能力与路由"已就位，"准入切换"
 * 仍是一次显式的部署动作。
 *
 * 名字的来路，逐个说清楚，避免下一位维护者以为是随手起的：
 *
 * - `sessionSchemaVersion` / `objectClass` / prefix 三项由 issue #855 §B 直接给定。
 * - `sourceFamily` 由契约名派生（`dm-session-ingest-2` 的家族就是
 *   `dm-session-ingest`），不另造一个词。
 * - `artifactSchemaVersion` 取 `dm-raw-recording-1`：与 object class `dataminer-raw`
 *   和 `raw/v2` 前缀同一个词根，且不含任何环境/训练词。旧名 `dm-local-dev-recording-*`
 *   的 `local-dev` 正是本 issue 要拆掉的那三件事粘在一起的写法。
 * - `preflightResult` 取 `preflight_pass`：旧值 `dev_non_trainable_pass` 把车道名写进了
 *   结果里。它只回答"preflight 过了没有"，不该兼职表达车道。
 * - **没有 `audio` 世代分裂**。legacy 车道用两个 session schema（`-2` / `-3`）区分
 *   有无音频；§B 明确 v2 只有 `dm-session-ingest-2` 一个值作为唯一路由键，所以
 *   "这场采集有没有音频"在 v2 上是 artifact 集合的事实，不是 schema 世代。
 */
const productionIngestV2: AdmissionFamily = {
  sourceFamily: "dm-session-ingest",
  contractVersion: "dm-1",
  artifactSchemaVersion: "dm-raw-recording-1",
  sessionSchemaVersion: "dm-session-ingest-2",
  audioDeclaration: "per_clip",
  storageNamespace: "raw",
  objectClass: "dataminer-raw",
  prefixPattern: /^raw\/v2\/[a-z0-9][a-z0-9-]{2,63}\/*$/,
  prefixHint: "raw/v2/<deployment>",
  // 没有默认值：新车道必须由部署显式命名自己的前缀，否则整条车道不出现。
  defaultPrefix: null,
  catalogMode: null,
  preflightResult: "preflight_pass",
  // 上限是 candidate，**不是**通过。blocker 全部清零后才由 clip_trainability
  // 判 is_trainable（issue #855 §C）。
  bestClassification: "training_candidate",
  admission: {
    sourceFamily: "dm-session-ingest",
    contractVersion: "dm-1",
    artifactSchemaVersion: "dm-raw-recording-1",
    environment: "production",
    storageNamespace: "raw",
  },
  retiredClassificationKeys: ["nonTrainable", "isTrainable", "productionCertified", "catalogMode"],
  sessionArtifactKeyStyle: "session_scoped",
};

const staging: EnvironmentProfile = {
  environment: "staging",
  // Staging deliberately has no first-party family: staging is non-trainable at
  // the source by policy, and rehearsing the consented lane there would mean
  // rehearsing with fake consent rows.
  families: [stagingDev],
};

const production: EnvironmentProfile = {
  environment: "production",
  // families[0] 是主家族，其前缀来自 R2_PREFIX；它今天仍是唯一被 `loadConfig`
  // 准入的家族（config.ts）。consented first-party 那条（docs/49 E7）随 docs/57
  // §3.3 的通道退役走了；certified 那条随平行授权车道退役（docs/100）。
  //
  // 中性摄取车道（issue #855 §B）定义在此，由 `R2_PREFIX_INGEST_V2` 显式准入。
  // 列出来而不是藏在别处，是因为它确实是 production 的一条准入家族——把它放在
  // 定义准入家族的这张表以外，就等于让每个读者重新发明一遍词表。
  families: [productionDev, productionIngestV2],
};

const profiles: Record<Environment, EnvironmentProfile> = { staging, production };

export function environmentProfile(environment: Environment): EnvironmentProfile {
  return profiles[environment];
}

/** The primary family — the one R2_PREFIX names and legacy call sites mean. */
export function primaryFamily(profile: EnvironmentProfile): AdmissionFamily {
  const family = profile.families[0];
  if (!family) throw new Error(`${profile.environment} has no admission families`);
  return family;
}

/**
 * v1 / v2 并存期的**唯一**存储路由键（issue #855 §B）。
 *
 * 删掉 `catalogMode` 之后 lane 不能靠猜，也不能再换一个环境词续命。唯一的键是服务端
 * 已经校验并持久化的 `sessions.schema_version`：它在 session 创建时一次写入、之后
 * 不可变，run / clip / multipart completion / session artifact 全都读库里的这个值
 * 选存储策略，不再读 Catalog 身份，也不接受后续请求重新声明。
 *
 * | `sessions.schema_version` | 策略 |
 * |---|---|
 * | `dm-local-dev-session-2` / `-3` | legacy v1：`raw/v1/non-trainable/...`、legacy object class、分类永久 `non_trainable` |
 * | `dm-session-ingest-2` | v2：`raw/v2/<deployment>/sessions/...`、`dataminer-raw`，服务端再派生分类 |
 *
 * `dm-local-dev-session-1` 不在表内：那一代没有 catalog 证据，也没有对应的准入家族
 * （它的行在库里只作为历史存在）。未知版本明确拒绝，不落到任何默认车道。
 *
 * 注意它按 **profile**（部署定义的全部家族）解析，而不是按 `admittedFamilies`：
 * "这条 session 属于哪条车道"是一个历史事实，不随部署配置漂移。"这条车道现在还
 * 准入吗"是另一个问题，由 `IngestionService` 的车道表回答，答案为否时 409
 * `lane_not_admitted`——存量 session 宁可上传停住，也不换一条前缀继续写。
 */
export function storagePolicyForSessionSchema(
  profile: EnvironmentProfile,
  sessionSchemaVersion: string,
): AdmissionFamily {
  for (const family of profile.families) {
    if (
      family.sessionSchemaVersion === sessionSchemaVersion ||
      family.audio?.sessionSchemaVersion === sessionSchemaVersion
    ) {
      return family;
    }
  }
  throw new Error(
    `unsupported session schema version ${sessionSchemaVersion} in ${profile.environment}; ` +
      "storage routing accepts only a persisted sessions.schema_version this deployment defines (issue #855 §B)",
  );
}

/**
 * 会话级快照对象的 key（`session.json`、`catalog.json`、`markers.jsonl` 等）。
 *
 * 两种形状见 [`AdmissionFamily.sessionArtifactKeyStyle`]。clip 对象的 key 由
 * `clipArtifactObjectKey` 生成，它本来就已经是 `<prefix>/sessions/<id>/runs/...`，
 * 因此 v2 的 `session_scoped` 形状让同一个 session 的**全部**对象落在
 * `raw/v2/<deployment>/sessions/<session-id>/` 之下，与 issue #855 §B 的布局逐字对上。
 */
export function sessionArtifactObjectKey(
  prefix: string,
  sessionId: string,
  fileName: string,
  style: AdmissionFamily["sessionArtifactKeyStyle"] = "legacy_flat",
): string {
  return style === "session_scoped"
    ? `${prefix}/sessions/${sessionId}/session/${fileName}`
    : `${prefix}/${sessionId}/session/${fileName}`;
}
