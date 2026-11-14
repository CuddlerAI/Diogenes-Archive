import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { loadConfig } from "./config.js";

const base = {
  CONSOLE_ENV: "staging",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_staging",
  CONSOLE_PUBLIC_ORIGIN: "https://console-staging.diogenesmodel.com",
  CONSOLE_IP_HASH_SALT: "a".repeat(48),
  CONSOLE_MAIL_MODE: "resend",
  CONSOLE_MAIL_API_KEY: "re_example",
  CONSOLE_MAIL_ALLOWLIST: "diogenesmodel.com",
  CONSOLE_PROXY_TOKEN: "b".repeat(48),
} satisfies NodeJS.ProcessEnv;

describe("console configuration", () => {
  // Production is enabled, and it is strictly stricter than staging. Each of
  // these is a way a production console could otherwise start while behaving
  // like a staging one.
  const productionBase = {
    ...base,
    CONSOLE_ENV: "production",
    CONSOLE_PUBLIC_ORIGIN: "https://console.diogenesmodel.com",
    CONSOLE_PROXY_TOKEN: "p".repeat(48),
    CONSOLE_MAIL_ALLOWLIST: "",
  } satisfies NodeJS.ProcessEnv;

  it("starts in production with the full configuration", () => {
    const config = loadConfig(productionBase);

    expect(config.environment).toBe("production");
    expect(config.proxyToken).toHaveLength(48);
    // No allowlist in production: real operators are on real addresses.
    expect(config.mail.allowlist).toEqual([]);
  });

  it("requires the proxy token in production, as it already does in staging", () => {
    const { CONSOLE_PROXY_TOKEN: _dropped, ...withoutToken } = productionBase;

    expect(() => loadConfig(withoutToken)).toThrow(
      /CONSOLE_PROXY_TOKEN must be set in production/,
    );
  });

  it("requires https for the public origin in production", () => {
    expect(() =>
      loadConfig({ ...productionBase, CONSOLE_PUBLIC_ORIGIN: "http://console.diogenesmodel.com" }),
    ).toThrow(/must be https/);
  });

  it("refuses to write verification codes to disk in production", () => {
    const { CONSOLE_MAIL_API_KEY: _dropped, ...withoutKey } = productionBase;

    expect(() => loadConfig({ ...withoutKey, CONSOLE_MAIL_MODE: "capture" })).toThrow(
      /only allowed in development/,
    );
  });

  it("caps the production session lifetime", () => {
    expect(() =>
      loadConfig({ ...productionBase, CONSOLE_SESSION_ABSOLUTE_MINUTES: "1440" }),
    ).toThrow(/must not exceed 12 hours in production/);
  });

  it("requires a first-party sender in production", () => {
    expect(() =>
      loadConfig({ ...productionBase, CONSOLE_MAIL_FROM: "no-reply@example.com" }),
    ).toThrow(/diogenesmodel\.com sender in production/);
  });

  it("refuses to write verification codes to disk outside development", () => {
    const { CONSOLE_MAIL_API_KEY: _unused, ...withoutKey } = base;
    expect(() => loadConfig({ ...withoutKey, CONSOLE_MAIL_MODE: "capture" })).toThrow(
      /only allowed in development/,
    );
  });

  it("requires a mail allowlist in staging so operator mail cannot reach arbitrary recipients", () => {
    expect(() => loadConfig({ ...base, CONSOLE_MAIL_ALLOWLIST: "" })).toThrow(/CONSOLE_MAIL_ALLOWLIST/);
  });

  it("requires an API key when delivery is live", () => {
    const { CONSOLE_MAIL_API_KEY: _unused, ...withoutKey } = base;
    expect(() => loadConfig(withoutKey)).toThrow(/CONSOLE_MAIL_API_KEY is required/);
  });

  it("defaults login to single-factor password_only and takes password_otp explicitly", () => {
    expect(loadConfig(base).loginMode).toBe("password_only");
    expect(loadConfig({ ...base, CONSOLE_LOGIN_MODE: "password_otp" }).loginMode).toBe(
      "password_otp",
    );
  });

  it("requires the Access issuer, audience, and admin allowlist when login is cf_access", () => {
    expect(() => loadConfig({ ...base, CONSOLE_LOGIN_MODE: "cf_access" })).toThrow(/cf_access requires/);

    const config = loadConfig({
      ...base,
      CONSOLE_LOGIN_MODE: "cf_access",
      CONSOLE_CF_ACCESS_TEAM_DOMAIN: "hakkoai.cloudflareaccess.com",
      CONSOLE_CF_ACCESS_AUD: "a".repeat(64),
      CONSOLE_CF_ACCESS_ADMIN_EMAILS: "Ops@Diogenesmodel.com, ops2@diogenesmodel.com",
    });
    expect(config.loginMode).toBe("cf_access");
    expect(config.cfAccess).toMatchObject({
      teamDomain: "hakkoai.cloudflareaccess.com",
      adminEmails: ["ops@diogenesmodel.com", "ops2@diogenesmodel.com"],
    });
  });

  it("rejects a placeholder ip hash salt", () => {
    expect(() => loadConfig({ ...base, CONSOLE_IP_HASH_SALT: `replace-me-${"x".repeat(32)}` })).toThrow();
  });

  it("requires a proxy token in staging so the console cannot be reached around the Worker", () => {
    const { CONSOLE_PROXY_TOKEN: _unused, ...withoutToken } = base;
    expect(() => loadConfig(withoutToken)).toThrow(/CONSOLE_PROXY_TOKEN must be set in staging/);
  });

  it("rejects a placeholder proxy token", () => {
    expect(() => loadConfig({ ...base, CONSOLE_PROXY_TOKEN: `replace-me-${"x".repeat(32)}` })).toThrow();
  });

  it("rejects a proxy token short enough to guess", () => {
    expect(() => loadConfig({ ...base, CONSOLE_PROXY_TOKEN: "short" })).toThrow();
  });

  // Local development must stay runnable with no secret at all; null is the
  // explicit "gate is open" state rather than an empty string that compares equal
  // to a missing header.
  it("leaves the proxy token null in development when it is unset", () => {
    const { CONSOLE_PROXY_TOKEN: _unused, ...withoutToken } = base;
    const config = loadConfig({
      ...withoutToken,
      CONSOLE_ENV: "development",
      CONSOLE_MAIL_MODE: "capture",
      CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    });
    expect(config.proxyToken).toBeNull();
  });

  // The client address is derived from this and nothing else, so a default that
  // trusted the whole forwarded chain would hand every caller its own rate limit
  // bucket on a surface that is reachable directly from the internet.
  it("trusts exactly one proxy hop unless told otherwise", () => {
    expect(loadConfig(base).trustedProxyHops).toBe(1);
    expect(loadConfig({ ...base, CONSOLE_TRUSTED_PROXY_HOPS: "2" }).trustedProxyHops).toBe(2);
  });

  it("refuses a hop count that is not a plausible number of proxies", () => {
    expect(() => loadConfig({ ...base, CONSOLE_TRUSTED_PROXY_HOPS: "-1" })).toThrow();
    expect(() => loadConfig({ ...base, CONSOLE_TRUSTED_PROXY_HOPS: "99" })).toThrow();
  });

  it("requires https for the public origin outside development", () => {
    expect(() =>
      loadConfig({ ...base, CONSOLE_PUBLIC_ORIGIN: "http://console-staging.diogenesmodel.com" }),
    ).toThrow(/must be https/);
  });

  it("keeps sudo inside the idle session window", () => {
    expect(() =>
      loadConfig({ ...base, CONSOLE_SUDO_MINUTES: "45", CONSOLE_SESSION_IDLE_MINUTES: "30" }),
    ).toThrow(/must not outlive the idle session window/);
  });

  it("rejects an idle window longer than the absolute session lifetime", () => {
    expect(() =>
      loadConfig({
        ...base,
        CONSOLE_SESSION_IDLE_MINUTES: "600",
        CONSOLE_SESSION_ABSOLUTE_MINUTES: "300",
      }),
    ).toThrow(/must not exceed CONSOLE_SESSION_ABSOLUTE_MINUTES/);
  });

  it("normalises the allowlist and public origin", () => {
    const config = loadConfig({
      ...base,
      CONSOLE_MAIL_ALLOWLIST: " Diogenesmodel.com , ops@example.com ,",
      CONSOLE_PUBLIC_ORIGIN: "https://console-staging.diogenesmodel.com/path",
    });

    expect(config.mail.allowlist).toEqual(["diogenesmodel.com", "ops@example.com"]);
    expect(config.publicOrigin).toBe("https://console-staging.diogenesmodel.com");
  });

  it("leaves the annotator surface off when none of its variables are set", () => {
    expect(loadConfig(base).app).toBeNull();
  });

  // The body is written to a temp file so the hash the config demands is the
  // hash of bytes that actually exist — which is the invariant under test.
  const termsBody = "DataMiner contributor terms, test fixture.\n";
  const termsHash = createHash("sha256").update(termsBody, "utf8").digest("hex");
  const termsPath = path.join(tmpdir(), `dm-terms-${termsHash.slice(0, 12)}.txt`);
  writeFileSync(termsPath, termsBody, "utf8");

  const appEnv = {
    CONSOLE_APP_DATABASE_URL: "postgres://app:secret@127.0.0.1:5432/dataminer_staging",
    CONSOLE_APP_TERMS_POLICY_VERSION: "2026-07-01",
    CONSOLE_APP_TERMS_PURPOSE: "staging capture",
    CONSOLE_APP_TERMS_HASH: termsHash,
    CONSOLE_APP_TERMS_BODY_PATH: termsPath,
  };

  it("enables the annotator surface only with the full set", () => {
    const config = loadConfig({ ...base, ...appEnv });
    expect(config.app).toEqual({
      databaseUrl: appEnv.CONSOLE_APP_DATABASE_URL,
      terms: {
        policyVersion: "2026-07-01",
        purpose: "staging capture",
        termsHash,
        body: termsBody,
        allowsTrainableUse: false,
        // Unset means the deployment never said what its terms authorize
        // (docs/108 §3.2). Empty rather than a guess, for the same reason
        // allowsTrainableUse defaults false.
        scopes: [],
      },
    });
  });

  it("normalises the configured terms scopes into a set", () => {
    const config = loadConfig({
      ...base,
      ...appEnv,
      CONSOLE_APP_TERMS_SCOPES: " video , input ,video,public_release ",
    });
    // Sorted and deduplicated at load, so nothing downstream can make two equal
    // scope sets compare unequal because of how the env var was typed.
    expect(config.app!.terms.scopes).toEqual(["input", "public_release", "video"]);
  });

  it("refuses to start on a scope word outside the vocabulary", () => {
    // A typo caught here is a deployment that fails to boot; a typo caught at
    // INSERT is a person who already clicked "I agree" being told the server is
    // misconfigured.
    expect(() =>
      loadConfig({ ...base, ...appEnv, CONSOLE_APP_TERMS_SCOPES: "input,micrphone" }),
    ).toThrow(/unknown scope/);
  });

  it("stamps signup consents trainable only when the terms decision is configured", () => {
    // docs/49 E2: the flag rides the terms version, defaults false, and only an
    // explicit deployment value flips it.
    expect(loadConfig({ ...base, ...appEnv }).app!.terms.allowsTrainableUse).toBe(false);
    expect(
      loadConfig({ ...base, ...appEnv, CONSOLE_APP_TERMS_ALLOWS_TRAINABLE: "true" }).app!.terms
        .allowsTrainableUse,
    ).toBe(true);
    expect(loadConfig(base).governance.allowTrainable).toBe(false);
    expect(
      loadConfig({ ...base, CONSOLE_GOVERNANCE_ALLOW_TRAINABLE: "true" }).governance.allowTrainable,
    ).toBe(true);
  });

  // The hash is what gets stamped onto every consent row, and the body is what
  // the user is shown. If they can disagree, consent records a document nobody
  // saw — so startup refuses rather than serving the pair.
  it("refuses terms whose hash does not match the served body", () => {
    expect(() =>
      loadConfig({ ...base, ...appEnv, CONSOLE_APP_TERMS_HASH: "c".repeat(64) }),
    ).toThrow(/does not match the sha256/);
  });

  it("refuses a terms body path that cannot be read", () => {
    expect(() =>
      loadConfig({
        ...base,
        ...appEnv,
        CONSOLE_APP_TERMS_BODY_PATH: path.join(tmpdir(), "dm-terms-does-not-exist.txt"),
      }),
    ).toThrow(/could not be read/);
  });

  // All-or-nothing: a connection string without terms would serve signup with a
  // blank policy, so a partial configuration fails startup instead.
  it("rejects an annotator surface configured without its terms", () => {
    const { CONSOLE_APP_TERMS_HASH: _dropped, ...missingHash } = appEnv;
    expect(() => loadConfig({ ...base, ...missingHash })).toThrow(
      /needs CONSOLE_APP_DATABASE_URL and all four CONSOLE_APP_TERMS/,
    );
  });

  it("rejects a terms hash that is not a sha256 digest", () => {
    expect(() => loadConfig({ ...base, ...appEnv, CONSOLE_APP_TERMS_HASH: "not-a-digest" })).toThrow();
  });

  // 绑定前通道（docs/37 §Bootstrap）。这个变量**没有任何部署脚本设过**，所以默认
  // 值就是生产行为——它曾经默认 `certified`，而 certified 信封没有任何客户端验得过
  // 签名（客户端只钉 `DEV_CATALOG_ANCHOR` 一把公钥）。docs/100 §11.2。
  it("defaults the pre-binding bootstrap channel to the only channel clients can verify", () => {
    expect(loadConfig(base).catalogBootstrapChannel).toBe("dev");
  });

  // 退役的渠道在**启动时**就炸，且说得出为什么——不静默改判成 dev。一个还写着
  // `=certified` 的部署，被静默改判只会让"为什么客户端装不上"变成一次盲查。
  it("refuses to start on a retired bootstrap channel instead of silently using dev", () => {
    expect(() =>
      loadConfig({ ...base, CONSOLE_CATALOG_BOOTSTRAP_CHANNEL: "certified" }),
    ).toThrow(/certified catalog channel was retired/);
  });
});
