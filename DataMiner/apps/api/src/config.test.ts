import { describe, expect, it } from "vitest";

import { admittedClassifications, loadConfig } from "./config.js";
import { primaryFamily } from "./environment.js";

const validEnvironment: NodeJS.ProcessEnv = {
  DATAMINER_ENV: "staging",
  DATABASE_URL: "postgresql://dataminer:secret@localhost:5432/dataminer",
  R2_ACCOUNT_ID: "account-id",
  R2_BUCKET: "dataminer-staging",
  R2_ACCESS_KEY_ID: "access-key",
  R2_SECRET_ACCESS_KEY: "secret-key",
};

// Production is a real environment now, so what used to be one blanket refusal
// becomes a set of explicit requirements. Every one of these is a way a
// production deployment could otherwise start while quietly behaving like
// staging.
const productionEnvironment: NodeJS.ProcessEnv = {
  ...validEnvironment,
  DATAMINER_ENV: "production",
  R2_BUCKET: "diogenes-model-training",
  R2_PREFIX: "raw/v1/non-trainable/dataminer-api-production",
  RETENTION_DAYS: "365",
  DATAMINER_PROXY_TOKEN: "p".repeat(48),
};

describe("loadConfig", () => {
  it("starts in production with an explicit prefix and retention", () => {
    const config = loadConfig(productionEnvironment);

    expect(config.environment).toBe("production");
    expect(primaryFamily(config.profile).storageNamespace).toBe("raw");
    expect(primaryFamily(config.profile).catalogMode).toBe("dev_non_trainable");
    expect(config.r2.prefix).toBe("raw/v1/non-trainable/dataminer-api-production");
    expect(config.retentionDays).toBe(365);
    expect(config.proxyToken).toHaveLength(48);
  });

  // The Cloud Run origin behind api.diogenesmodel.com must admit `allUsers` —
  // desktop clients carry a device token, not a Google identity — so the edge
  // header is the only thing making the bare `*.run.app` URL useless. A
  // production process that starts without it is silently bypassable.
  it("refuses to start in production without the edge proxy token", () => {
    const { DATAMINER_PROXY_TOKEN: _dropped, ...withoutToken } = productionEnvironment;

    expect(() => loadConfig(withoutToken)).toThrow(
      /DATAMINER_PROXY_TOKEN must be set in production/,
    );
  });

  // certified 车道随 docs/100 退役：R2_PREFIX_CERTIFIED / DATAMINER_CAPTURE_GRANT_*
  // 都已从 schema 移除。zod 忽略未知变量，所以一份还没清理干净的部署 env 不会
  // 阻止启动——但它也**开不出**第二条准入家族，这正是这条断言要钉住的。
  it("ignores the retired certified lane variables instead of admitting a second family", () => {
    const config = loadConfig({
      ...productionEnvironment,
      R2_PREFIX_CERTIFIED: "raw/v1/certified/dataminer-api-production",
      DATAMINER_CAPTURE_GRANT_KMS_KEY: "projects/p/locations/l/keyRings/r/cryptoKeys/grant/1",
    });
    expect(config.admittedFamilies).toHaveLength(1);
    expect(config.admittedFamilies[0]!.family.sourceFamily).toBe("dm-local-dev");
    expect(admittedClassifications(config)).toEqual(["non_trainable"]);
  });

  it("rejects a short or placeholder edge proxy token", () => {
    expect(() => loadConfig({ ...productionEnvironment, DATAMINER_PROXY_TOKEN: "short" })).toThrow();
    expect(() =>
      loadConfig({ ...productionEnvironment, DATAMINER_PROXY_TOKEN: `replace-me-${"x".repeat(32)}` }),
    ).toThrow();
  });

  it("leaves the edge gate optional in staging, where there is no Worker in front", () => {
    expect(loadConfig(validEnvironment).proxyToken).toBeNull();
    expect(
      loadConfig({ ...validEnvironment, DATAMINER_PROXY_TOKEN: "s".repeat(48) }).proxyToken,
    ).toHaveLength(48);
  });

  it("refuses to inherit a prefix in production", () => {
    const { R2_PREFIX: _dropped, ...withoutPrefix } = productionEnvironment;

    expect(() => loadConfig(withoutPrefix)).toThrow(/R2_PREFIX is required in production/);
  });

  it("refuses a staging prefix in production and a production prefix in staging", () => {
    expect(() =>
      loadConfig({
        ...productionEnvironment,
        R2_PREFIX: "raw-staging/v1/non-trainable/dataminer-api-staging",
      }),
    ).toThrow(/raw\/v1\/non-trainable/);
    expect(() =>
      loadConfig({ ...validEnvironment, R2_PREFIX: "raw/v1/non-trainable/dataminer-api-production" }),
    ).toThrow(/raw-staging\/v1\/non-trainable/);
  });

  it("admits only the dev lane until the first-party prefix is configured", () => {
    const config = loadConfig(productionEnvironment);

    expect(config.admittedFamilies).toHaveLength(1);
    expect(config.admittedFamilies[0]!.family.sourceFamily).toBe("dm-local-dev");
    expect(admittedClassifications(config)).toEqual(["non_trainable"]);
  });

  // R2_PREFIX_FIRST_PARTY 被**显式拒绝**而不是静默忽略（docs/57 §3.3）：留着
  // 这个环境变量会让运维以为那条车道还开着，而实际上写进去的数据没有任何准入
  // 家族会收。三个环境一视同仁。
  it("refuses R2_PREFIX_FIRST_PARTY everywhere: the lane retired", () => {
    for (const base of [validEnvironment, productionEnvironment]) {
      expect(() =>
        loadConfig({ ...base, R2_PREFIX_FIRST_PARTY: "raw/v1/first-party/dataminer-api" }),
      ).toThrow(/no longer supported/);
    }
  });

  it("requires retention to be stated in production", () => {
    const { RETENTION_DAYS: _dropped, ...withoutRetention } = productionEnvironment;

    expect(() => loadConfig(withoutRetention)).toThrow(/RETENTION_DAYS is required in production/);
  });

  it("caps retention at the consent ceiling", () => {
    expect(() => loadConfig({ ...productionEnvironment, RETENTION_DAYS: "400" })).toThrow();
  });

  it("rejects the staging-only retention name outside staging", () => {
    const { RETENTION_DAYS: _dropped, ...withoutRetention } = productionEnvironment;

    expect(() =>
      loadConfig({ ...withoutRetention, STAGING_RETENTION_DAYS: "30" }),
    ).toThrow(/staging-only name/);
  });

  it("keeps the staging default so existing deployments are unaffected", () => {
    const config = loadConfig(validEnvironment);

    expect(config.environment).toBe("staging");
    expect(config.r2.prefix).toBe("raw-staging/v1/non-trainable/dataminer-api-staging");
    expect(config.retentionDays).toBe(30);
    expect(primaryFamily(config.profile).bestClassification).toBe("non_trainable");
  });

  // The shared ingest secret is gone (11 §10). Startup must not resurrect it by
  // accident: an environment that still carries the old variable is simply
  // ignored, and nothing in AppConfig can hand a caller a fleet-wide credential.
  it("no longer accepts or requires a shared staging token", () => {
    const config = loadConfig({
      ...validEnvironment,
      DATAMINER_STAGING_TOKEN: "s".repeat(32),
    });
    expect(config).not.toHaveProperty("stagingToken");
  });

  it("rejects an object prefix outside raw-staging", () => {
    expect(() =>
      loadConfig({ ...validEnvironment, R2_PREFIX: "raw/v1/non-trainable" }),
    ).toThrow(/exclusive deployment prefix/);
  });

  it("rejects the shared non-trainable root because orphan cleanup requires exclusive ownership", () => {
    expect(() =>
      loadConfig({ ...validEnvironment, R2_PREFIX: "raw-staging/v1/non-trainable" }),
    ).toThrow(/exclusive deployment prefix/);
  });

  it("keeps the staging-only namespace and removes trailing slashes", () => {
    const config = loadConfig({
      ...validEnvironment,
      R2_PREFIX: "raw-staging/v1/non-trainable/dataminer-api-staging///",
    });

    expect(config.environment).toBe("staging");
    expect(config.r2.prefix).toBe(
      "raw-staging/v1/non-trainable/dataminer-api-staging",
    );
  });

  it("rejects worker leases that cannot cover their fenced provider calls", () => {
    expect(() =>
      loadConfig({
        ...validEnvironment,
        R2_REQUEST_TIMEOUT_SECONDS: "120",
        MULTIPART_COMPLETION_LEASE_SECONDS: "299",
      }),
    ).toThrow(/two R2 requests/);
    expect(() =>
      loadConfig({
        ...validEnvironment,
        R2_REQUEST_TIMEOUT_SECONDS: "120",
        PURGE_LEASE_SECONDS: "179",
      }),
    ).toThrow(/one R2 request/);
  });
});
