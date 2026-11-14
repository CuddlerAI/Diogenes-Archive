import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

import { z } from "zod";

// 渠道词表与 HTTP 契约同源：一个 `=certified` 的部署要在启动时炸掉，用的
// 必须是发布路由拒绝它时同一条理由，而不是另一份手抄的词表。
import { catalogChannelSchema } from "./governance-contracts.js";

/**
 * What a consent may authorize (docs/108 §3.2).
 *
 * Kept byte-identical to the vocabulary CHECK in migration 0030. The database
 * is the last line, not the first: a word that only fails at INSERT time fails
 * after a user already clicked "I agree", which is far too late to be told the
 * deployment was misconfigured.
 */
export const TERMS_SCOPE_VOCABULARY = [
  "audio",
  "depth",
  "input",
  "public_release",
  "trainable_research",
  "video",
] as const;

export type TermsScope = (typeof TERMS_SCOPE_VOCABULARY)[number];

/**
 * Comma separated config to a sorted, deduplicated set.
 *
 * Sorting here rather than at every call site is what lets the rest of the code
 * treat scopes as a set: ordering and repetition carry no meaning, so they must
 * not be able to make two equal sets look different.
 */
export function parseTermsScopes(raw: string): string[] {
  const entries = raw
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
  const unknown = entries.filter(
    (entry) => !(TERMS_SCOPE_VOCABULARY as readonly string[]).includes(entry),
  );
  if (unknown.length > 0) {
    throw new Error(
      `CONSOLE_APP_TERMS_SCOPES contains unknown scope(s): ${[...new Set(unknown)].sort().join(", ")}. Allowed: ${TERMS_SCOPE_VOCABULARY.join(", ")}`,
    );
  }
  return [...new Set(entries)].sort();
}

const environmentSchema = z.object({
  CONSOLE_ENV: z.enum(["development", "staging", "production"]),
  DATABASE_URL: z.string().url(),
  HOST: z.string().default("127.0.0.1"),
  PORT: z.coerce.number().int().min(1).max(65_535).default(8090),
  CONSOLE_PUBLIC_ORIGIN: z.string().url(),
  CONSOLE_IP_HASH_SALT: z
    .string()
    .min(32)
    .refine((value) => !/^(replace|change-?me|example)/i.test(value), {
      message: "must be a generated secret, not an example placeholder",
    }),

  // The shared secret the console Worker stamps onto every request it forwards
  // (docs/11 §4.3). It gates `/console/v1/*` only — the desktop client reaches
  // `/app/v1/*` directly, without the Worker, so a service-wide gate would lock
  // out the annotator surface. Same "must be generated" discipline as the IP
  // salt: a placeholder token is worse than none, because it reads as protection.
  CONSOLE_PROXY_TOKEN: z
    .string()
    .min(32)
    .refine((value) => !/^(replace|change-?me|example)/i.test(value), {
      message: "must be a generated secret, not an example placeholder",
    })
    .optional(),

  // How many proxy hops in front of this service are ours, counted from the
  // service inwards. Everything derived from the client address — the per-IP
  // rate limit buckets, `annotator_invite_uses.ip_hash`, the audit trail —
  // reads whatever this resolves to, so getting it wrong is not cosmetic.
  //
  // Trusting the whole X-Forwarded-For chain (which is what `trustProxy: true`
  // does, and what the first cut of `clientAddress` did by taking the leftmost
  // entry) is only safe behind a proxy that overwrites the header. `/app/v1` is
  // reached directly by the desktop client, and Google's front end *appends*
  // rather than replaces — so a client that sends its own X-Forwarded-For puts
  // a value of its choosing on the left, and every per-IP limit becomes
  // per-whatever-the-caller-typed. Counting from the right instead means the
  // attacker can only prepend entries that are never read.
  //
  // 1 matches Cloud Run reached directly, which is how `/app/v1` is reached.
  //
  // This is the depth of the *direct* surface only: how many proxies we own
  // sit in front of a request that did NOT traverse the Cloudflare Worker
  // (default 1 = Cloud Run reached directly). Requests that did traverse the
  // Worker — `/console/v1` always, `/app/v1` when the desktop client uses the
  // console hostname — sit one hop deeper, but that extra hop is NOT expressed
  // here: `clientAddress` (http.ts) steps one entry further left only when the
  // request carries the Worker's `x-console-proxy-token`, which the Worker
  // attaches to everything it forwards and strips from everything inbound.
  // Traversal is attributed by that evidence, never by path and never by
  // raising this number.
  //
  // So: do not raise this to sharpen limiting on Worker-fronted routes — the
  // token already does that exactly, and raising it makes every DIRECT caller
  // resolve to an entry they prepended themselves. Raise it only when a proxy
  // of ours is genuinely added in front of the direct Cloud Run path itself.
  //
  // The floor is 1, not 0. Zero is not "even coarser" — Fastify treats a falsy
  // trustProxy as off entirely, which drops back to the socket address (it can
  // be undefined, and hashing undefined throws) and also stops
  // `x-forwarded-proto`/`-host` from being honoured. There is no deployment of
  // this service with nothing in front of it.
  CONSOLE_TRUSTED_PROXY_HOPS: z.coerce.number().int().min(1).max(8).default(1),

  // Login layering (docs/20 §6.A, §6.D). `password_only` (the default) is
  // single-factor: a correct password issues a session directly, and the email
  // OTP is kept only for invite and password-reset. `password_otp` is the
  // two-step fallback — password, then a mandatory email OTP. `cf_access`
  // delegates authentication entirely to a Cloudflare Access layer in front of
  // the console: the console verifies the signed Access JWT and issues a session
  // for the matching operator, so passing Access is the single sign-in and there
  // is no console password step at all.
  CONSOLE_LOGIN_MODE: z.enum(["password_otp", "password_only", "cf_access"]).default("password_only"),
  // Required only when CONSOLE_LOGIN_MODE=cf_access. The team domain is the JWT
  // issuer (e.g. hakkoai.cloudflareaccess.com); the AUD is the Access
  // application's Application Audience tag; the admin list is the console-side
  // allowlist of emails that may be admitted (defence in depth over the Access
  // policy itself), and the first email a JIT-provisioned operator is created as.
  CONSOLE_CF_ACCESS_TEAM_DOMAIN: z
    .string()
    .trim()
    .regex(/^[a-z0-9-]+\.cloudflareaccess\.com$/i, "must be a <team>.cloudflareaccess.com domain")
    .optional(),
  CONSOLE_CF_ACCESS_AUD: z.string().trim().regex(/^[0-9a-f]{64}$/, "must be the 64-hex Access AUD tag").optional(),
  CONSOLE_CF_ACCESS_ADMIN_EMAILS: z.string().default(""),

  CONSOLE_MAIL_MODE: z.enum(["capture", "resend"]),
  CONSOLE_MAIL_FROM: z.string().email().default("no-reply@mail.diogenesmodel.com"),
  CONSOLE_MAIL_CAPTURE_DIR: z.string().default("./.mail-outbox"),
  CONSOLE_MAIL_API_KEY: z.string().min(1).optional(),
  CONSOLE_MAIL_ALLOWLIST: z.string().default(""),

  CONSOLE_SESSION_ABSOLUTE_MINUTES: z.coerce.number().int().min(5).max(1_440).default(720),
  CONSOLE_SESSION_IDLE_MINUTES: z.coerce.number().int().min(5).max(720).default(60),
  CONSOLE_SUDO_MINUTES: z.coerce.number().int().min(1).max(60).default(15),

  // Annotator sessions slide instead of expiring on a clock (docs/34 §6.1a,
  // decision ⑤ "signed in until they sign out"): each authenticated use pushes
  // expiry back out to this window, so only real inactivity ends a session.
  // Revocation — logout, password change/reset, suspension — is untouched.
  // The operator session knobs above deliberately do not apply here: the
  // governance surface keeps its 12h absolute / 60min idle friction.
  CONSOLE_APP_SESSION_SLIDING_DAYS: z.coerce.number().int().min(1).max(365).default(30),

  CONSOLE_OTP_TTL_MINUTES: z.coerce.number().int().min(1).max(60).default(10),
  CONSOLE_OTP_MAX_ATTEMPTS: z.coerce.number().int().min(1).max(10).default(5),
  CONSOLE_INVITE_TTL_HOURS: z.coerce.number().int().min(1).max(168).default(24),
  CONSOLE_PASSWORD_RESET_TTL_MINUTES: z.coerce.number().int().min(5).max(240).default(30),
  CONSOLE_PASSWORD_MIN_LENGTH: z.coerce.number().int().min(12).max(128).default(12),

  CONSOLE_OTP_SEND_PER_MINUTE: z.coerce.number().int().min(1).max(10).default(1),
  CONSOLE_OTP_SEND_PER_HOUR: z.coerce.number().int().min(1).max(100).default(5),
  CONSOLE_OTP_SEND_PER_DAY: z.coerce.number().int().min(1).max(500).default(20),
  CONSOLE_OTP_SEND_PER_IP_HOUR: z.coerce.number().int().min(1).max(500).default(10),
  CONSOLE_LOGIN_PER_IP_HOUR: z.coerce.number().int().min(1).max(1_000).default(30),
  CONSOLE_LOGIN_PER_EMAIL_HOUR: z.coerce.number().int().min(1).max(1_000).default(30),
  CONSOLE_LOGIN_LOCK_THRESHOLD: z.coerce.number().int().min(3).max(100).default(10),
  CONSOLE_LOGIN_LOCK_BASE_MINUTES: z.coerce.number().int().min(1).max(240).default(15),
  CONSOLE_LOGIN_LOCK_MAX_MINUTES: z.coerce.number().int().min(15).max(1_440).default(240),

  // Read-only object access for clip preview. Absent means preview is simply
  // unavailable rather than degraded: no write credential is ever accepted here.
  CONSOLE_R2_ACCOUNT_ID: z.string().min(1).optional(),
  CONSOLE_R2_BUCKET: z.string().min(1).optional(),
  CONSOLE_R2_ACCESS_KEY_ID: z.string().min(1).optional(),
  CONSOLE_R2_SECRET_ACCESS_KEY: z.string().min(1).optional(),
  // Comma-separated: one ingest lane writes under one prefix, and a deployment
  // that admits the certified lane as well as the non-trainable one has two
  // (docs/67 §3.1). A single-value setting still parses — it is a list of one.
  CONSOLE_R2_PREFIX: z.string().min(1).optional(),
  // Read-only object access for the data-review surface (docs/33 §3.1). A
  // second, independent credential from clip preview: scoped to exactly the
  // review buckets and revocable without touching preview or ingest. Absent
  // means the review surface is simply not served.
  CONSOLE_REVIEW_R2_ACCOUNT_ID: z.string().min(1).optional(),
  CONSOLE_REVIEW_R2_ACCESS_KEY_ID: z.string().min(1).optional(),
  CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY: z.string().min(1).optional(),
  // JSON array of sources; the prefixes are a hard allowlist — nothing outside
  // them is ever listed, headed, fetched, or signed, whatever the caller asks.
  CONSOLE_REVIEW_SOURCES: z.string().min(1).optional(),
  CONSOLE_REVIEW_TTL_SECONDS: z.coerce.number().int().min(30).max(900).default(300),
  // HF release reconciliation (docs/33 §6). Read-only fine-grained token; the
  // repos list is a hard allowlist — reconcile refuses anything not on it, so
  // the token cannot be steered at arbitrary repos.
  CONSOLE_HF_TOKEN: z.string().min(1).optional(),
  CONSOLE_HF_REPOS: z.string().min(1).optional(),
  // 会话验收报告的只读对象访问（docs/136 §4）。命名遵契约原文 ACCEPTANCE_R2_*
  // 而不是 CONSOLE_ACCEPTANCE_R2_*。缺省未配置时验收路由答 503（明确「存储未
  // 配置」），绝不伪装成 404「未验收」。bucket 有缺省值所以不在 all-or-none
  // 组里；三个凭据字段同 preview/review 的纪律：全给或全不给。
  // 部署映射（GCP Secret Manager, project hakkolite）：
  //   ACCEPTANCE_R2_ACCOUNT_ID        ← dataminer-console-prod-r2-account
  //   ACCEPTANCE_R2_ACCESS_KEY_ID     ← dataminer-console-prod-r2-key
  //   ACCEPTANCE_R2_SECRET_ACCESS_KEY ← dataminer-console-prod-r2-secret
  ACCEPTANCE_R2_ACCOUNT_ID: z.string().min(1).optional(),
  ACCEPTANCE_R2_ACCESS_KEY_ID: z.string().min(1).optional(),
  ACCEPTANCE_R2_SECRET_ACCESS_KEY: z.string().min(1).optional(),
  ACCEPTANCE_R2_BUCKET: z.string().min(1).default("diogenes-model-training"),
  // D1 观测镜像的发布目标。未配置时 outbox 会积压——积压看得见，静默丢弃看不见。
  CONSOLE_MIRROR_D1_ACCOUNT_ID: z.string().min(1).optional(),
  CONSOLE_MIRROR_D1_DATABASE_ID: z.string().min(1).optional(),
  CONSOLE_MIRROR_D1_API_TOKEN: z.string().min(1).optional(),
  CONSOLE_MIRROR_DRAIN_SECONDS: z.coerce.number().int().min(5).max(3_600).default(30),
  CONSOLE_MIRROR_DRAIN_BATCH: z.coerce.number().int().min(1).max(500).default(100),
  CONSOLE_PREVIEW_TTL_SECONDS: z.coerce.number().int().min(30).max(900).default(300),

  // The annotator surface (docs/11). Its own connection string so it runs under
  // its own database role (dataminer_app_staging) — the isolation in §4.2 is a
  // separate role, not just a separate pool on the same credentials. Unset means
  // the surface is not served at all. The three terms values are what consent
  // rows are stamped with; they are frozen per deployment (§13 open question 1)
  // and required together with the connection string so an enabled surface can
  // never record consent against a blank policy.
  CONSOLE_APP_DATABASE_URL: z.string().url().optional(),
  CONSOLE_APP_TERMS_POLICY_VERSION: z.string().trim().min(1).max(64).optional(),
  CONSOLE_APP_TERMS_PURPOSE: z.string().trim().min(1).max(200).optional(),
  CONSOLE_APP_TERMS_HASH: z.string().regex(/^[0-9a-f]{64}$/).optional(),
  // The text those three describe. It has to be served, not just hashed: the
  // client shows it at signup, and consent to a document nobody can read is not
  // consent. Startup checks that its sha256 equals CONSOLE_APP_TERMS_HASH, so
  // the bytes users accept and the hash stamped onto their consent row cannot
  // drift apart — before this the hash was hand-configured and nothing tied it
  // to any file.
  CONSOLE_APP_TERMS_BODY_PATH: z.string().trim().min(1).optional(),
  // docs/49 E2: whether consent rows stamped with the configured terms version
  // authorize trainable use. Off by default; enabling it is the deployment-time
  // assertion that the served terms text really grants that use (the
  // 2026-07-25-teaser terms do — §3 public release, §4 CC-BY-NC-4.0). A new
  // terms version resets to false until the decision is re-made explicitly.
  CONSOLE_APP_TERMS_ALLOWS_TRAINABLE: z.enum(["true", "false"]).default("false"),
  // docs/108 §3.2: which capture scopes the served terms authorize, comma
  // separated. Stamped onto every consent signed against this terms version and
  // never backfilled — the whole point is that "what does this document
  // authorize" is answered once, at signing time, from the deployment that
  // served the document.
  //
  // Empty by default, and deliberately so: an unset value means the deployment
  // never declared what its terms cover, which is exactly what should be
  // reported rather than guessed. Every word must be in the vocabulary below,
  // which is the same list migration 0030 puts in a CHECK — a typo is a startup
  // failure, not a consent row that authorizes nothing anybody can explain.
  CONSOLE_APP_TERMS_SCOPES: z.string().default(""),
  // docs/49 E7: whether operator governance writes may issue trainable consents
  // and grants. Off by default — the historical behaviour where every trainable
  // request was policy_blocked. Enabling it is the explicit unlock that the
  // prerequisites (consent closure, first-party lane) are in place.
  CONSOLE_GOVERNANCE_ALLOW_TRAINABLE: z.enum(["true", "false"]).default("false"),
  // Dedicated least-privilege connection used only by the catalog publisher.
  // Unset means the console exposes no publish route.
  CONSOLE_CATALOG_PUBLISH_DATABASE_URL: z.string().url().optional(),
  // 绑定前启动通道下发哪个渠道（docs/37 §Bootstrap）。
  //
  // 曾经默认 `certified`，理由是"猜错方向的代价不对称"。那个理由随 certified
  // 车道退役失效并**反转**：客户端只钉了 `DEV_CATALOG_ANCHOR` 一把公钥
  // （`catalog.rs`），certified 信封没有任何客户端验得过。这个默认值没有任何
  // 部署脚本覆盖过，于是生产 console 的绑定前通道一直指着一条谁都装不上的
  // 渠道（issue #829 §1）。退成单值 `dev`。
  //
  // 变量保留而不是硬编码：一个仍然显式写 `=certified` 的部署必须在**启动时**
  // 就炸掉并说明原因，而不是被静默改判成 dev。
  CONSOLE_CATALOG_BOOTSTRAP_CHANNEL: catalogChannelSchema.default("dev"),
});

// One browsable bucket for the data-review surface. `dbAuthority` says whether
// this console's own database is the registration authority for the bucket:
// "production" cross-checks artifact_objects, "none" means the authority lives
// elsewhere (the e2e bucket registers in the isolated e2e database) and the UI
// must not present that as an unregistered-object finding.
const reviewSourceSchema = z.object({
  id: z
    .string()
    .regex(/^[a-z0-9][a-z0-9-]{0,31}$/, "source ids are 1-32 chars of [a-z0-9-]"),
  bucket: z.string().min(1).max(128),
  prefixes: z
    .array(
      z
        .string()
        .min(2)
        .max(256)
        .refine((value) => value.endsWith("/"), { message: "prefixes must end with '/'" })
        .refine((value) => !value.startsWith("/"), { message: "prefixes must be relative" })
        .refine((value) => !value.includes(".."), { message: "prefixes must not contain '..'" }),
    )
    .min(1),
  dbAuthority: z.enum(["production", "none"]).default("none"),
});
const reviewSourcesSchema = z.array(reviewSourceSchema).min(1);
export type ReviewSource = z.infer<typeof reviewSourceSchema>;

/**
 * Split `CONSOLE_R2_PREFIX` into the lane prefixes the deployment may sign
 * under (docs/67 §3.1). Trailing slashes are stripped so the guard can append
 * exactly one; `..` is rejected outright rather than normalised, because a
 * prefix that climbs is a configuration mistake and quietly repairing it would
 * hide which lane the operator actually meant.
 */
function previewPrefixes(raw: string): string[] {
  const prefixes = raw
    .split(",")
    .map((entry) => entry.trim().replace(/\/+$/, ""))
    .filter((entry) => entry.length > 0);
  if (prefixes.length === 0) {
    throw new Error("CONSOLE_R2_PREFIX must name at least one prefix");
  }
  for (const prefix of prefixes) {
    if (prefix.startsWith("/") || prefix.includes("..")) {
      throw new Error(`CONSOLE_R2_PREFIX entry "${prefix}" must be relative and must not contain '..'`);
    }
  }
  return prefixes;
}

export type ConsoleConfig = {
  environment: "development" | "staging" | "production";
  databaseUrl: string;
  host: string;
  port: number;
  publicOrigin: string;
  ipHashSalt: string;
  /** Null means the `/console/v1/*` proxy gate is open. Only development may
   * leave it null; staging startup refuses to come up without it. */
  proxyToken: string | null;
  /** Proxy hops in front of this service, counted inwards. Feeds Fastify's
   * `trustProxy`, which is the only thing that decides the client address. */
  trustedProxyHops: number;
  loginMode: "password_otp" | "password_only" | "cf_access";
  /** Present only when loginMode is cf_access. */
  cfAccess: { teamDomain: string; aud: string; adminEmails: string[] } | null;
  mail: {
    mode: "capture" | "resend";
    from: string;
    captureDir: string;
    apiKey: string | null;
    allowlist: string[];
  };
  session: {
    absoluteMinutes: number;
    idleMinutes: number;
    sudoMinutes: number;
  };
  appSession: {
    /** docs/34 §6.1a: sliding-renewal window for annotator sessions, in days. */
    slidingDays: number;
  };
  challenge: {
    otpTtlMinutes: number;
    otpMaxAttempts: number;
    inviteTtlHours: number;
    passwordResetTtlMinutes: number;
  };
  password: {
    minLength: number;
  };
  preview: {
    accountId: string;
    bucket: string;
    accessKeyId: string;
    secretAccessKey: string;
    /** Every prefix the deployment will sign under; never empty when preview is
     * configured. One entry per admitted ingest lane (docs/67 §3.1). */
    prefixes: string[];
    ttlSeconds: number;
  } | null;
  review: {
    accountId: string;
    accessKeyId: string;
    secretAccessKey: string;
    sources: ReviewSource[];
    ttlSeconds: number;
  } | null;
  hf: {
    token: string;
    repos: string[];
  } | null;
  /** 会话验收报告读取（docs/136 §4）。null = 未配置，路由答 503。 */
  acceptance: {
    accountId: string;
    accessKeyId: string;
    secretAccessKey: string;
    bucket: string;
  } | null;
  mirror: {
    accountId: string;
    databaseId: string;
    apiToken: string;
    drainSeconds: number;
    drainBatch: number;
  } | null;
  limits: {
    otpSendPerMinute: number;
    otpSendPerHour: number;
    otpSendPerDay: number;
    otpSendPerIpHour: number;
    loginPerIpHour: number;
    loginPerEmailHour: number;
    lockThreshold: number;
    lockBaseMinutes: number;
    lockMaxMinutes: number;
  };
  app: {
    databaseUrl: string;
    terms: {
      policyVersion: string;
      purpose: string;
      termsHash: string;
      body: string;
      /** docs/49 E2: served-terms-version decision, deployment-configured. */
      allowsTrainableUse: boolean;
      /**
       * docs/108 §3.2: what the served document authorizes. Sorted and
       * deduplicated at load so every consent stamped from it compares by set
       * semantics without callers having to normalise first.
       */
      scopes: string[];
    };
  } | null;
  governance: {
    /** docs/49 E7: operator writes may issue trainable consent/grants. */
    allowTrainable: boolean;
  };
  catalogPublishDatabaseUrl: string | null;
  catalogBootstrapChannel: "dev";
};

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): ConsoleConfig {
  const parsed = environmentSchema.parse(environment);
  // Production is not staging with a different label. Everything that staging
  // merely recommends, production requires, and the checks below are what make
  // the difference real rather than documented.
  const isProduction = parsed.CONSOLE_ENV === "production";
  const isDeployed = parsed.CONSOLE_ENV !== "development";

  if (parsed.CONSOLE_MAIL_MODE === "capture" && parsed.CONSOLE_ENV !== "development") {
    throw new Error(
      "CONSOLE_MAIL_MODE=capture writes verification codes to disk and is only allowed in development",
    );
  }
  if (parsed.CONSOLE_MAIL_MODE === "resend" && !parsed.CONSOLE_MAIL_API_KEY) {
    throw new Error("CONSOLE_MAIL_API_KEY is required when CONSOLE_MAIL_MODE=resend");
  }
  if (parsed.CONSOLE_SESSION_IDLE_MINUTES > parsed.CONSOLE_SESSION_ABSOLUTE_MINUTES) {
    throw new Error(
      "CONSOLE_SESSION_IDLE_MINUTES must not exceed CONSOLE_SESSION_ABSOLUTE_MINUTES",
    );
  }
  if (parsed.CONSOLE_SUDO_MINUTES > parsed.CONSOLE_SESSION_IDLE_MINUTES) {
    throw new Error("CONSOLE_SUDO_MINUTES must not outlive the idle session window");
  }
  if (parsed.CONSOLE_OTP_SEND_PER_MINUTE > parsed.CONSOLE_OTP_SEND_PER_HOUR) {
    throw new Error("CONSOLE_OTP_SEND_PER_MINUTE must not exceed the hourly allowance");
  }
  if (parsed.CONSOLE_OTP_SEND_PER_HOUR > parsed.CONSOLE_OTP_SEND_PER_DAY) {
    throw new Error("CONSOLE_OTP_SEND_PER_HOUR must not exceed the daily allowance");
  }
  if (parsed.CONSOLE_LOGIN_LOCK_BASE_MINUTES > parsed.CONSOLE_LOGIN_LOCK_MAX_MINUTES) {
    throw new Error("CONSOLE_LOGIN_LOCK_BASE_MINUTES must not exceed the maximum lock");
  }

  // Optional in development so `pnpm dev` needs no secret, mandatory in every
  // deployed environment. The alternative — optional everywhere — is how this
  // gate spent its first release doing nothing: the Worker stamped the header,
  // nothing read it, and both wrangler.toml and the Worker comment described it
  // as already enforced.
  if (isDeployed && !parsed.CONSOLE_PROXY_TOKEN) {
    throw new Error(
      `CONSOLE_PROXY_TOKEN must be set in ${parsed.CONSOLE_ENV} so /console/v1 cannot be reached by bypassing the Worker`,
    );
  }

  const allowlist = parsed.CONSOLE_MAIL_ALLOWLIST.split(",")
    .map((entry) => entry.trim().toLowerCase())
    .filter((entry) => entry.length > 0);
  if (parsed.CONSOLE_ENV === "staging" && allowlist.length === 0) {
    throw new Error(
      "CONSOLE_MAIL_ALLOWLIST must be set in staging so operator mail cannot reach arbitrary recipients",
    );
  }
  // Production deliberately has no allowlist requirement: real operators live on
  // real addresses. Everything else tightens instead.
  if (isProduction) {
    if (parsed.CONSOLE_SESSION_ABSOLUTE_MINUTES > 720) {
      throw new Error("CONSOLE_SESSION_ABSOLUTE_MINUTES must not exceed 12 hours in production");
    }
    if (!parsed.CONSOLE_MAIL_FROM.endsWith("diogenesmodel.com")) {
      throw new Error("CONSOLE_MAIL_FROM must be a diogenesmodel.com sender in production");
    }
  }

  const previewFields = [
    parsed.CONSOLE_R2_ACCOUNT_ID,
    parsed.CONSOLE_R2_BUCKET,
    parsed.CONSOLE_R2_ACCESS_KEY_ID,
    parsed.CONSOLE_R2_SECRET_ACCESS_KEY,
    parsed.CONSOLE_R2_PREFIX,
  ];
  const configuredPreviewFields = previewFields.filter((field) => field !== undefined).length;
  if (configuredPreviewFields !== 0 && configuredPreviewFields !== previewFields.length) {
    throw new Error(
      "clip preview needs every CONSOLE_R2_* value or none: a partial configuration would silently disable it",
    );
  }
  const preview =
    configuredPreviewFields === previewFields.length
      ? {
          accountId: parsed.CONSOLE_R2_ACCOUNT_ID!,
          bucket: parsed.CONSOLE_R2_BUCKET!,
          accessKeyId: parsed.CONSOLE_R2_ACCESS_KEY_ID!,
          secretAccessKey: parsed.CONSOLE_R2_SECRET_ACCESS_KEY!,
          prefixes: previewPrefixes(parsed.CONSOLE_R2_PREFIX!),
          ttlSeconds: parsed.CONSOLE_PREVIEW_TTL_SECONDS,
        }
      : null;

  // Same discipline as preview: every CONSOLE_REVIEW_* value or none. The
  // sources document is part of the group — credentials with no allowlist would
  // be a working key with nowhere it is allowed to look, which reads as
  // configured while serving nothing.
  const reviewFields = [
    parsed.CONSOLE_REVIEW_R2_ACCOUNT_ID,
    parsed.CONSOLE_REVIEW_R2_ACCESS_KEY_ID,
    parsed.CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY,
    parsed.CONSOLE_REVIEW_SOURCES,
  ];
  const configuredReviewFields = reviewFields.filter((field) => field !== undefined).length;
  if (configuredReviewFields !== 0 && configuredReviewFields !== reviewFields.length) {
    throw new Error(
      "the data-review surface needs every CONSOLE_REVIEW_* value or none: a partial configuration would silently disable it",
    );
  }
  let review: ConsoleConfig["review"] = null;
  if (configuredReviewFields === reviewFields.length) {
    let sourcesJson: unknown;
    try {
      sourcesJson = JSON.parse(parsed.CONSOLE_REVIEW_SOURCES!);
    } catch {
      throw new Error("CONSOLE_REVIEW_SOURCES must be valid JSON");
    }
    const sources = reviewSourcesSchema.parse(sourcesJson);
    const ids = new Set(sources.map((source) => source.id));
    if (ids.size !== sources.length) {
      throw new Error("CONSOLE_REVIEW_SOURCES ids must be unique");
    }
    review = {
      accountId: parsed.CONSOLE_REVIEW_R2_ACCOUNT_ID!,
      accessKeyId: parsed.CONSOLE_REVIEW_R2_ACCESS_KEY_ID!,
      secretAccessKey: parsed.CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY!,
      sources,
      ttlSeconds: parsed.CONSOLE_REVIEW_TTL_SECONDS,
    };
  }

  // HF reconciliation: token and repo allowlist arrive together or not at all.
  const hfFields = [parsed.CONSOLE_HF_TOKEN, parsed.CONSOLE_HF_REPOS];
  const configuredHfFields = hfFields.filter((field) => field !== undefined).length;
  if (configuredHfFields !== 0 && configuredHfFields !== hfFields.length) {
    throw new Error(
      "HF reconciliation needs both CONSOLE_HF_TOKEN and CONSOLE_HF_REPOS or neither",
    );
  }
  let hf: ConsoleConfig["hf"] = null;
  if (configuredHfFields === hfFields.length) {
    let reposJson: unknown;
    try {
      reposJson = JSON.parse(parsed.CONSOLE_HF_REPOS!);
    } catch {
      throw new Error("CONSOLE_HF_REPOS must be valid JSON");
    }
    const repos = z
      .array(z.string().regex(/^[\w.-]+\/[\w.-]+$/, "repo ids look like org/name"))
      .min(1)
      .parse(reposJson);
    hf = { token: parsed.CONSOLE_HF_TOKEN!, repos };
  }

  // 验收报告凭据：同一条全给或全不给纪律。bucket 不在组里——它有缺省值
  // （diogenes-model-training），单独出现不构成半套配置。
  const acceptanceFields = [
    parsed.ACCEPTANCE_R2_ACCOUNT_ID,
    parsed.ACCEPTANCE_R2_ACCESS_KEY_ID,
    parsed.ACCEPTANCE_R2_SECRET_ACCESS_KEY,
  ];
  const configuredAcceptanceFields = acceptanceFields.filter((field) => field !== undefined).length;
  if (configuredAcceptanceFields !== 0 && configuredAcceptanceFields !== acceptanceFields.length) {
    throw new Error(
      "session acceptance needs every ACCEPTANCE_R2_* credential or none: a partial configuration would silently disable it",
    );
  }
  const acceptance =
    configuredAcceptanceFields === acceptanceFields.length
      ? {
          accountId: parsed.ACCEPTANCE_R2_ACCOUNT_ID!,
          accessKeyId: parsed.ACCEPTANCE_R2_ACCESS_KEY_ID!,
          secretAccessKey: parsed.ACCEPTANCE_R2_SECRET_ACCESS_KEY!,
          bucket: parsed.ACCEPTANCE_R2_BUCKET,
        }
      : null;

  // 与 preview 同一条纪律：全给或全不给。半套配置会让镜像看起来接上了，
  // 实际上每次发布都失败，而失败只体现为一条日志。
  const mirrorFields = [
    parsed.CONSOLE_MIRROR_D1_ACCOUNT_ID,
    parsed.CONSOLE_MIRROR_D1_DATABASE_ID,
    parsed.CONSOLE_MIRROR_D1_API_TOKEN,
  ];
  const configuredMirrorFields = mirrorFields.filter((field) => field !== undefined).length;
  if (configuredMirrorFields !== 0 && configuredMirrorFields !== mirrorFields.length) {
    throw new Error(
      "the D1 credential mirror needs every CONSOLE_MIRROR_D1_* value or none: a partial configuration would silently stop publishing",
    );
  }
  const mirror =
    configuredMirrorFields === mirrorFields.length
      ? {
          accountId: parsed.CONSOLE_MIRROR_D1_ACCOUNT_ID!,
          databaseId: parsed.CONSOLE_MIRROR_D1_DATABASE_ID!,
          apiToken: parsed.CONSOLE_MIRROR_D1_API_TOKEN!,
          drainSeconds: parsed.CONSOLE_MIRROR_DRAIN_SECONDS,
          drainBatch: parsed.CONSOLE_MIRROR_DRAIN_BATCH,
        }
      : null;

  const publicOrigin = new URL(parsed.CONSOLE_PUBLIC_ORIGIN);
  if (isDeployed && publicOrigin.protocol !== "https:") {
    throw new Error("CONSOLE_PUBLIC_ORIGIN must be https outside development");
  }

  // The annotator surface is all-or-nothing: its connection string and its three
  // terms values arrive together, so an enabled surface can never stamp consent
  // against a blank policy. A partial configuration fails startup rather than
  // quietly serving signup with no terms.
  const appFields = [
    parsed.CONSOLE_APP_DATABASE_URL,
    parsed.CONSOLE_APP_TERMS_POLICY_VERSION,
    parsed.CONSOLE_APP_TERMS_PURPOSE,
    parsed.CONSOLE_APP_TERMS_HASH,
    parsed.CONSOLE_APP_TERMS_BODY_PATH,
  ];
  const configuredAppFields = appFields.filter((field) => field !== undefined).length;
  if (configuredAppFields !== 0 && configuredAppFields !== appFields.length) {
    throw new Error(
      "the annotator surface needs CONSOLE_APP_DATABASE_URL and all four CONSOLE_APP_TERMS_* values or none",
    );
  }
  let termsBody: string | null = null;
  if (configuredAppFields === appFields.length) {
    // Read and verified at startup, not per request: a signup that could not
    // show the terms should never have been servable in the first place, and
    // this is the only place the configured hash is tied to actual bytes.
    try {
      termsBody = readFileSync(parsed.CONSOLE_APP_TERMS_BODY_PATH!, "utf8");
    } catch {
      throw new Error("CONSOLE_APP_TERMS_BODY_PATH could not be read");
    }
    const actual = createHash("sha256").update(termsBody, "utf8").digest("hex");
    if (actual !== parsed.CONSOLE_APP_TERMS_HASH) {
      throw new Error(
        "CONSOLE_APP_TERMS_HASH does not match the sha256 of CONSOLE_APP_TERMS_BODY_PATH",
      );
    }
  }
  const termsScopes = parseTermsScopes(parsed.CONSOLE_APP_TERMS_SCOPES);
  const app =
    configuredAppFields === appFields.length
      ? {
          databaseUrl: parsed.CONSOLE_APP_DATABASE_URL!,
          terms: {
            policyVersion: parsed.CONSOLE_APP_TERMS_POLICY_VERSION!,
            purpose: parsed.CONSOLE_APP_TERMS_PURPOSE!,
            termsHash: parsed.CONSOLE_APP_TERMS_HASH!,
            body: termsBody!,
            allowsTrainableUse: parsed.CONSOLE_APP_TERMS_ALLOWS_TRAINABLE === "true",
            scopes: termsScopes,
          },
        }
      : null;

  // cf_access delegates sign-in to the Cloudflare Access layer, so it cannot run
  // without the issuer, the audience, and at least one admissible email. A mode
  // that trusted an unset issuer would trust anyone.
  const cfAccessAdminEmails = parsed.CONSOLE_CF_ACCESS_ADMIN_EMAILS.split(",")
    .map((entry) => entry.trim().toLowerCase())
    .filter((entry) => entry.length > 0);
  let cfAccess: { teamDomain: string; aud: string; adminEmails: string[] } | null = null;
  if (parsed.CONSOLE_LOGIN_MODE === "cf_access") {
    if (!parsed.CONSOLE_CF_ACCESS_TEAM_DOMAIN || !parsed.CONSOLE_CF_ACCESS_AUD || cfAccessAdminEmails.length === 0) {
      throw new Error(
        "CONSOLE_LOGIN_MODE=cf_access requires CONSOLE_CF_ACCESS_TEAM_DOMAIN, CONSOLE_CF_ACCESS_AUD, and a non-empty CONSOLE_CF_ACCESS_ADMIN_EMAILS",
      );
    }
    cfAccess = {
      teamDomain: parsed.CONSOLE_CF_ACCESS_TEAM_DOMAIN.toLowerCase(),
      aud: parsed.CONSOLE_CF_ACCESS_AUD.toLowerCase(),
      adminEmails: cfAccessAdminEmails,
    };
  }

  return {
    environment: parsed.CONSOLE_ENV,
    databaseUrl: parsed.DATABASE_URL,
    host: parsed.HOST,
    port: parsed.PORT,
    publicOrigin: publicOrigin.origin,
    ipHashSalt: parsed.CONSOLE_IP_HASH_SALT,
    proxyToken: parsed.CONSOLE_PROXY_TOKEN ?? null,
    trustedProxyHops: parsed.CONSOLE_TRUSTED_PROXY_HOPS,
    loginMode: parsed.CONSOLE_LOGIN_MODE,
    cfAccess,
    mail: {
      mode: parsed.CONSOLE_MAIL_MODE,
      from: parsed.CONSOLE_MAIL_FROM,
      captureDir: parsed.CONSOLE_MAIL_CAPTURE_DIR,
      apiKey: parsed.CONSOLE_MAIL_API_KEY ?? null,
      allowlist,
    },
    session: {
      absoluteMinutes: parsed.CONSOLE_SESSION_ABSOLUTE_MINUTES,
      idleMinutes: parsed.CONSOLE_SESSION_IDLE_MINUTES,
      sudoMinutes: parsed.CONSOLE_SUDO_MINUTES,
    },
    appSession: {
      slidingDays: parsed.CONSOLE_APP_SESSION_SLIDING_DAYS,
    },
    challenge: {
      otpTtlMinutes: parsed.CONSOLE_OTP_TTL_MINUTES,
      otpMaxAttempts: parsed.CONSOLE_OTP_MAX_ATTEMPTS,
      inviteTtlHours: parsed.CONSOLE_INVITE_TTL_HOURS,
      passwordResetTtlMinutes: parsed.CONSOLE_PASSWORD_RESET_TTL_MINUTES,
    },
    password: {
      minLength: parsed.CONSOLE_PASSWORD_MIN_LENGTH,
    },
    preview,
    review,
    hf,
    acceptance,
    mirror,
    limits: {
      otpSendPerMinute: parsed.CONSOLE_OTP_SEND_PER_MINUTE,
      otpSendPerHour: parsed.CONSOLE_OTP_SEND_PER_HOUR,
      otpSendPerDay: parsed.CONSOLE_OTP_SEND_PER_DAY,
      otpSendPerIpHour: parsed.CONSOLE_OTP_SEND_PER_IP_HOUR,
      loginPerIpHour: parsed.CONSOLE_LOGIN_PER_IP_HOUR,
      loginPerEmailHour: parsed.CONSOLE_LOGIN_PER_EMAIL_HOUR,
      lockThreshold: parsed.CONSOLE_LOGIN_LOCK_THRESHOLD,
      lockBaseMinutes: parsed.CONSOLE_LOGIN_LOCK_BASE_MINUTES,
      lockMaxMinutes: parsed.CONSOLE_LOGIN_LOCK_MAX_MINUTES,
    },
    app,
    governance: {
      allowTrainable: parsed.CONSOLE_GOVERNANCE_ALLOW_TRAINABLE === "true",
    },
    catalogPublishDatabaseUrl: parsed.CONSOLE_CATALOG_PUBLISH_DATABASE_URL ?? null,
    catalogBootstrapChannel: parsed.CONSOLE_CATALOG_BOOTSTRAP_CHANNEL,
  };
}
