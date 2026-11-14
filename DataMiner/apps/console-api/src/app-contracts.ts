import { z } from "zod";

import { normalize, normalizedPattern } from "./app-invites.js";

const jsonObject = z.record(z.string(), z.unknown());

export const appEmailSchema = z
  .string()
  .trim()
  .toLowerCase()
  .max(254)
  .refine((value) => /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(value), {
    message: "an email address is required",
  });

/**
 * The password floor matches the operator side. Length is the only rule: no
 * composition requirements, which push people toward predictable substitutions
 * without adding real entropy.
 */
export const appPasswordSchema = z.string().min(12).max(200);

/**
 * The client sends whatever the user typed. Normalization happens here, once, so
 * every downstream consumer sees the stored form — and the shape check runs on
 * the normalized value, because that is what the table's CHECK will see.
 */
export const inviteCodeSchema = z
  .string()
  .transform((value) => normalize(value))
  .refine((value) => normalizedPattern.test(value), {
    message: "an invite code is required",
  });

/**
 * Contact profile (docs/34 §5.4). Everything optional: the profile is
 * operational contact data, not identity, and an empty object is a legal
 * profile. `.strict()` so a client cannot smuggle fields the console would
 * then have to render. PUT semantics are whole-object replace — an omitted
 * field clears, which is what lets a user remove a stale phone number.
 */
export const profileSchema = z
  .object({
    displayName: z.string().trim().min(1).max(80).optional(),
    phone: z.string().trim().min(1).max(32).optional(),
    imKind: z.enum(["wechat", "qq", "telegram", "discord", "other"]).optional(),
    imHandle: z.string().trim().min(1).max(128).optional(),
    region: z.string().trim().min(1).max(64).optional(),
  })
  .strict()
  .refine((value) => (value.imKind === undefined) === (value.imHandle === undefined), {
    message: "imKind and imHandle must be provided together",
  });

export const signupSchema = z.object({
  inviteCode: inviteCodeSchema,
  email: appEmailSchema,
  password: appPasswordSchema,
  alias: z.string().trim().min(1).max(80),
  // The client confirms which terms it displayed. The server records its own
  // current version regardless — this field only lets the server notice that the
  // client was showing something stale and refuse (11 §7.1).
  acceptedPolicyVersion: z.string().trim().min(1).max(64),
  // Optional contact facts collected by the web signup form (docs/34 §5.1,
  // A12). Older desktop clients simply omit it — backward compatible by
  // construction.
  profile: profileSchema.optional(),
});

export const signupVerifySchema = z.object({
  challengeId: z.string().uuid(),
  code: z.string().trim().regex(/^[0-9]{6}$/, "a six digit code is required"),
});

export const signupResendSchema = z.object({
  challengeId: z.string().uuid(),
});

export const appLoginSchema = z.object({
  email: appEmailSchema,
  password: appPasswordSchema,
});

export const appPasswordResetRequestSchema = z.object({
  email: appEmailSchema,
});

/** Password change for a signed-in annotator (docs/34 §6.3). */
export const changePasswordSchema = z.object({
  currentPassword: z.string().min(1).max(200),
  newPassword: appPasswordSchema,
});

/**
 * Confirming an operator-issued claim link (docs/34 §5.2/§6.5). The token is
 * the 43-character opaque secret from the URL, not a six digit code. Terms and
 * profile only matter on the activation branch; the reset branch ignores them,
 * and which branch applies is decided server-side from the credential status.
 */
export const claimConfirmSchema = z.object({
  challengeId: z.string().uuid(),
  token: z.string().min(20).max(200),
  password: appPasswordSchema,
  acceptedPolicyVersion: z.string().trim().min(1).max(64).optional(),
  profile: profileSchema.optional(),
});

export const claimParamsSchema = z.object({
  challengeId: z.string().uuid(),
});

/**
 * The challenge id comes back from the request step; the code arrives by mail.
 * Both are required, so a leaked code alone is not enough — it has to be paired
 * with the id issued to the session that asked.
 */
export const appPasswordResetConfirmSchema = z.object({
  challengeId: z.string().uuid(),
  code: z.string().trim().regex(/^[0-9]{6}$/, "a six digit code is required"),
  password: appPasswordSchema,
});

/**
 * Re-signing the current terms. Only the version the client believes it showed
 * — everything else about the consent comes from deployment config, so a caller
 * cannot choose its own purpose, hash, or trainable-use answer.
 */
export const consentRenewalSchema = z.object({
  acceptedPolicyVersion: z.string().trim().min(1).max(64),
});

export const enrollDeviceSchema = z.object({
  clientDeviceId: z.string().trim().min(1).max(120),
  // Transitional compatibility for clients that enrolled before Issue #453.
  // Enrollment no longer grants profile authorization; when present this
  // well-formed legacy field is accepted and ignored.
  deviceProfile: z.object({
    id: z.string().trim().min(1).max(120),
    version: z.string().trim().min(1).max(64),
    contentHash: z.string().regex(/^sha256:[0-9a-f]{64}$/),
  }).optional(),
  windowsBuild: z.string().trim().min(1).max(64),
  gpuPciId: z.string().trim().min(1).max(120),
  gpuDriver: z.string().trim().min(1).max(64),
  clientVersion: z.string().trim().min(1).max(64),
  displaySnapshot: jsonObject,
  inputDevicesSnapshot: z.union([jsonObject, z.array(z.unknown())]),
});

/**
 * Client behaviour telemetry (docs/11 §8).
 *
 * `kind` is checked against a server-side allowlist rather than accepted freely,
 * so the client cannot invent event shapes the console then has to render. An
 * unknown kind is refused loudly instead of stored and forgotten.
 */
export const clientEventKinds = [
  "app_started",
  "recovery_completed",
  "target_bound",
  "session_started",
  "session_stopped",
  "run_started",
  "run_completed",
  "fault",
  "enrolled",
  "signed_in",
  // Structured capture facts (docs/23 §3.1). preflight_* is what makes
  // HARDWARE_PATH_PREFLIGHT_FAILED aggregable by build/driver in the warehouse.
  "preflight_failed",
  "preflight_passed",
  "update_check",
  "capture_rollup",
] as const;

/**
 * Per-kind details contracts (docs/23 §3.2).
 *
 * Every kind carries a `schema_version` literal and is `.strict()`: an unknown
 * field is refused at the boundary, not stored and rendered by surprise. The
 * lane/consentPresent stamps are runtime context the client declares — they
 * never override the identity columns the server resolves from the device
 * credential.
 */
/**
 * 两个键**同时**接受，这不是骑墙：
 *
 * - `sessionContract` 是 issue #855 §B 之后客户端盖的词（摄取契约的 source
 *   family）。它只说"这场采集属于哪一代契约"，不兼职表达环境或训练资格。
 * - `catalogMode` 是断代前的词。它必须继续被接受，因为**已经发出去的客户端仍在
 *   盖它**，而且它们本地 outbox 里可能还压着没送达的行。这里拒收等于把一批历史
 *   遥测永久丢掉——而遥测拒收的代价从来不该由「服务端先升级」来支付。
 *
 * 值域刻意保持原样（含两个已退役的车道名）：这条枚举描述的是**历史客户端会发什么**，
 * 不是今天还有哪些车道。收紧它只会把老包的事件挡在门外。
 */
const laneStamp = {
  catalogMode: z.enum(["certified", "dev_non_trainable", "first_party_release"]).optional(),
  sessionContract: z.enum(["dm-local-dev", "dm-session-ingest"]).optional(),
  consentPresent: z.boolean().optional(),
};

const strictDetails = <Version extends string, Shape extends z.ZodRawShape>(
  version: Version,
  shape: Shape,
) => z.object({ schema_version: z.literal(version), ...laneStamp, ...shape }).strict();

// preflight_failed / preflight_passed share one literal: they are the two
// outcomes of the same check, and the RCA query joins them by build.
const preflightShape = {
  checkId: z.string().trim().min(1).max(120).optional(),
  windowsBuild: z.string().trim().min(1).max(64),
  gpuVendor: z.string().trim().max(120).optional(),
  gpuDeviceId: z.string().trim().max(120).optional(),
  driverVersion: z.string().trim().max(64).optional(),
};

export const clientEventDetailsSchemas = {
  app_started: strictDetails("dm-client-app-started-1", {
    journalSchemaVersion: z.number().int().min(1).optional(),
    // Ring-buffer drops since the last report — makes local discarding visible
    // in the warehouse instead of silent (docs/23 §4.2).
    droppedEvents: z.number().int().min(0).optional(),
  }),
  recovery_completed: strictDetails("dm-client-recovery-1", {
    recoveredRuns: z.number().int().min(0).optional(),
    findings: z.number().int().min(0).optional(),
  }),
  target_bound: strictDetails("dm-client-target-bound-1", {}),
  session_started: strictDetails("dm-client-session-1", {}),
  session_stopped: strictDetails("dm-client-session-1", {}),
  run_started: strictDetails("dm-client-run-started-1", {
    runName: z.string().trim().min(1).max(64).optional(),
    encoderBackend: z.string().trim().max(64).optional(),
    fps: z.number().int().min(1).max(1000).optional(),
    width: z.number().int().min(1).optional(),
    height: z.number().int().min(1).optional(),
    segmentSeconds: z.number().int().min(1).optional(),
  }),
  run_completed: strictDetails("dm-client-run-completed-1", {
    runName: z.string().trim().min(1).max(64).optional(),
    reason: z.string().trim().max(120).optional(),
    durationSeconds: z.number().min(0).optional(),
    segments: z.number().int().min(0).optional(),
  }),
  fault: strictDetails("dm-client-fault-1", {
    code: z.string().trim().min(1).max(120),
    message: z.string().trim().max(2048).optional(),
    action: z.string().trim().max(120).optional(),
    retryable: z.boolean().optional(),
  }),
  enrolled: strictDetails("dm-client-enrolled-1", {}),
  signed_in: strictDetails("dm-client-signed-in-1", {}),
  preflight_failed: strictDetails("dm-client-preflight-1", {
    failureCode: z.string().trim().min(1).max(120),
    retryable: z.boolean().optional(),
    ...preflightShape,
  }),
  preflight_passed: strictDetails("dm-client-preflight-1", preflightShape),
  update_check: strictDetails("dm-client-update-check-1", {
    trigger: z.string().trim().min(1).max(64),
    currentVersion: z.string().trim().min(1).max(64),
    latestVersion: z.string().trim().max(64).optional(),
    updateAvailable: z.boolean().optional(),
    error: z.string().trim().max(2048).optional(),
  }),
  capture_rollup: strictDetails("dm-client-capture-rollup-1", {
    runName: z.string().trim().min(1).max(64),
    segmentSeq: z.number().int().min(0),
    durationSeconds: z.number().min(0).optional(),
    frameCount: z.number().int().min(0).optional(),
    queueDepthMax: z.number().int().min(0).optional(),
    storageState: z.string().trim().max(64).optional(),
  }),
} as const satisfies Record<(typeof clientEventKinds)[number], z.ZodType>;

export const clientEventSchema = z
  .object({
    eventId: z.string().uuid(),
    occurredAt: z.iso.datetime({ offset: true }),
    kind: z.enum(clientEventKinds),
    severity: z.enum(["debug", "info", "warn", "error"]),
    clientVersion: z.string().trim().min(1).max(64),
    sessionId: z.string().uuid().optional(),
    details: z.record(z.string(), z.unknown()).default({}),
  })
  .superRefine((event, context) => {
    const result = clientEventDetailsSchemas[event.kind].safeParse(event.details);
    for (const issue of result.error?.issues ?? []) {
      context.addIssue({ ...issue, path: ["details", ...issue.path] });
    }
  });

export const deviceIdParamsSchema = z.object({
  deviceId: z.string().uuid(),
});

export const clientEventBatchSchema = z.object({
  // Bounded so one client cannot post an unbounded batch; the client buffers
  // locally and sends in chunks.
  events: z.array(clientEventSchema).min(1).max(100),
});

/** Console-side invite administration (operator writes). */
export const createInviteSchema = z.object({
  count: z.coerce.number().int().min(1).max(100).default(1),
  note: z.string().trim().min(1).max(200).optional(),
  expiresAt: z.iso.datetime({ offset: true }).optional(),
});

export const inviteListQuerySchema = z.object({
  status: z.enum(["active", "revoked"]).optional(),
});

export const inviteCodeParamsSchema = z.object({
  code: z.string().trim().min(1).max(32),
});

/**
 * Operator-initiated registration (docs/34 §5.2). No invite code and no
 * password: the sudo-gated operator action *is* the authorization decision,
 * and the password is set by the annotator on the claim page — never here.
 * The note lands in the audit details only; an operations aside is not an
 * identity fact.
 */
export const annotatorRegistrationSchema = z.object({
  email: appEmailSchema,
  alias: z.string().trim().min(1).max(80),
  note: z.string().trim().min(1).max(256).optional(),
  profile: profileSchema.optional(),
});

export const adminProfileUpdateSchema = z.object({
  profile: profileSchema,
});

/**
 * Claim-link (re)issue for an existing annotator (docs/34 §5.2). The email is
 * required in the body: a governance-created annotator has no credential row
 * to read it from, and for a pending one the service checks it matches the
 * stored address rather than silently re-pointing the account.
 */
export const annotatorCredentialInviteSchema = z.object({
  email: appEmailSchema,
});

export type SignupInput = z.infer<typeof signupSchema>;
export type SignupVerifyInput = z.infer<typeof signupVerifySchema>;
export type SignupResendInput = z.infer<typeof signupResendSchema>;
export type AppLoginInput = z.infer<typeof appLoginSchema>;
export type ConsentRenewalInput = z.infer<typeof consentRenewalSchema>;
export type PasswordResetRequestInput = z.infer<typeof appPasswordResetRequestSchema>;
export type PasswordResetConfirmInput = z.infer<typeof appPasswordResetConfirmSchema>;
export type EnrollDeviceInput = z.infer<typeof enrollDeviceSchema>;
export type ProfileInput = z.infer<typeof profileSchema>;
export type ChangePasswordInput = z.infer<typeof changePasswordSchema>;
export type ClaimConfirmInput = z.infer<typeof claimConfirmSchema>;
export type AnnotatorRegistrationInput = z.infer<typeof annotatorRegistrationSchema>;
