import { beforeAll, describe, expect, it, vi } from "vitest";

import { AuthService, type RequestContext } from "./auth.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import type { ConsoleError } from "./errors.js";
import { RecipientNotAllowedError } from "./mailer.js";
import { hashPassword } from "./secrets.js";
import { argon2TestTimeoutMs, MemoryConsoleRepository, MemoryMailer } from "./testing.js";

// Almost every case below performs production Argon2 verification — the lockout
// ladders spend one per threshold attempt, and the enumeration cases spend an
// extra hash for the decoy. See argon2TestTimeoutMs for why the budget is this
// large and why the hashing itself must not be cheapened instead.
vi.setConfig({ testTimeout: argon2TestTimeoutMs });

const password = "correct horse battery staple";
let passwordHash = "";

const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "b".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
});

const context: RequestContext = {
  requestId: "req_test",
  ipHash: "c".repeat(64),
  userAgent: "vitest",
};

beforeAll(async () => {
  passwordHash = await hashPassword(password);
});

async function harness() {
  const repository = new MemoryConsoleRepository();
  const mailer = new MemoryMailer();
  const service = new AuthService(repository, mailer, config);
  const operator = await repository.createOperator({
    email: "ops@diogenesmodel.com",
    role: "operator",
    createdBy: null,
  });
  await repository.activateOperatorWithPassword(operator.id, passwordHash);
  return { repository, mailer, service, operatorId: operator.id };
}

async function signIn(harnessValue: Awaited<ReturnType<typeof harness>>) {
  const { challengeId } = await harnessValue.service.login(
    "ops@diogenesmodel.com",
    password,
    context,
  );
  const code = harnessValue.mailer.lastCode();
  return harnessValue.service.verifyOtp(challengeId, code, context);
}

function codeFor(actual: string): string {
  return actual === "000000" ? "111111" : "000000";
}

/**
 * Signs in as an admin and steps up, because administering operators is
 * sudo-gated. The session has to be re-read after the grant: `sudoActive` is
 * resolved when the session is loaded, not mutated on the object in hand.
 */
async function signInAsElevatedAdmin(harnessValue: Awaited<ReturnType<typeof harness>>) {
  await harnessValue.repository.setOperatorRole(harnessValue.operatorId, "admin");
  const grant = await signIn(harnessValue);
  const session = (await harnessValue.service.authenticate(grant.token))!;
  await harnessValue.service.grantSudo(session, { password }, context);
  return (await harnessValue.service.authenticate(grant.token))!;
}

describe("console login", () => {
  it("issues an unusable challenge for an unknown address so accounts cannot be enumerated", async () => {
    const { service, mailer } = await harness();

    const known = await service.login("ops@diogenesmodel.com", password, context);
    const unknown = await service.login("nobody@diogenesmodel.com", password, context);

    expect(Object.keys(unknown).sort()).toEqual(Object.keys(known).sort());
    expect(unknown.expiresInSeconds).toBe(known.expiresInSeconds);
    expect(unknown.challengeId).not.toBe(known.challengeId);
    expect(mailer.outbox).toHaveLength(1);
    await expect(service.verifyOtp(unknown.challengeId, "000000", context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
  });

  it("returns the same challenge shape for a wrong password and records the failure", async () => {
    const { service, repository, operatorId, mailer } = await harness();

    const result = await service.login("ops@diogenesmodel.com", "not the password", context);

    expect(result.challengeId).toMatch(/^[0-9a-f-]{36}$/);
    expect(mailer.outbox).toHaveLength(0);
    expect(repository.operators.get(operatorId)?.failedLoginCount).toBe(1);
    expect(repository.actionsFor("operator.login_rejected")[0]?.details).toMatchObject({
      reason: "password_mismatch",
    });
  });

  it("completes password then code and issues a session", async () => {
    const value = await harness();

    const grant = await signIn(value);

    expect(grant.token).toHaveLength(43);
    expect(grant.operator.id).toBe(value.operatorId);
    expect(value.repository.actionsFor("operator.session_created")).toHaveLength(1);
    expect(await value.service.authenticate(grant.token)).not.toBeNull();
  });

  it("never writes the verification code into an audit event", async () => {
    const value = await harness();
    await signIn(value);

    const code = value.mailer.lastCode();
    const serialised = JSON.stringify(value.repository.auditEvents);

    expect(serialised).not.toContain(code);
    expect(serialised).not.toContain(password);
  });

  it("locks the account once the failure threshold is reached", async () => {
    const { service, repository, operatorId } = await harness();

    for (let attempt = 0; attempt < config.limits.lockThreshold; attempt += 1) {
      await service.login("ops@diogenesmodel.com", "wrong", context);
    }

    const operator = repository.operators.get(operatorId);
    expect(operator?.lockedUntil).not.toBeNull();

    const locked = await service.login("ops@diogenesmodel.com", password, context);
    await expect(service.verifyOtp(locked.challengeId, "000000", context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
    expect(repository.actionsFor("operator.login_rejected").at(-1)?.details).toMatchObject({
      reason: "locked",
    });
  });

  it("refuses a suspended operator even with the right password", async () => {
    const { service, repository, operatorId, mailer } = await harness();
    await repository.setOperatorStatus(operatorId, "suspended");

    await service.login("ops@diogenesmodel.com", password, context);

    expect(mailer.outbox).toHaveLength(0);
    expect(repository.actionsFor("operator.login_rejected")[0]?.details).toMatchObject({
      reason: "status_suspended",
    });
  });

  it("stops delivery when the recipient is outside the allowlist", async () => {
    const { service, mailer, repository } = await harness();
    mailer.failWith = new RecipientNotAllowedError();

    await expect(service.login("ops@diogenesmodel.com", password, context)).rejects.toMatchObject({
      code: "recipient_not_allowed",
    });
    expect(repository.actionsFor("operator.mail_blocked")).toHaveLength(1);
  });

  it("rate limits verification mail per address", async () => {
    const { service } = await harness();

    await service.login("ops@diogenesmodel.com", password, context);
    const error = await service
      .login("ops@diogenesmodel.com", password, context)
      .catch((thrown: ConsoleError) => thrown);

    expect(error).toMatchObject({ code: "rate_limited", statusCode: 429 });
    expect((error as ConsoleError).retryAfterSeconds).toBeGreaterThan(0);
  });
});

describe("console login (password_only)", () => {
  const passwordOnlyConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "b".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_LOGIN_MODE: "password_only",
  });

  async function singleFactorHarness() {
    const repository = new MemoryConsoleRepository();
    const mailer = new MemoryMailer();
    const service = new AuthService(repository, mailer, passwordOnlyConfig);
    const operator = await repository.createOperator({
      email: "ops@diogenesmodel.com",
      role: "operator",
      createdBy: null,
    });
    await repository.activateOperatorWithPassword(operator.id, passwordHash);
    return { repository, mailer, service, operatorId: operator.id };
  }

  it("issues a session directly from a correct password, sending no code", async () => {
    const { service, repository, mailer, operatorId } = await singleFactorHarness();

    const grant = await service.loginPasswordOnly("ops@diogenesmodel.com", password, context);

    expect(grant.token).toHaveLength(43);
    expect(grant.operator.id).toBe(operatorId);
    expect(mailer.outbox).toHaveLength(0); // single factor: no OTP mail is sent
    expect(repository.operators.get(operatorId)?.lastLoginAt).not.toBeNull();
    expect(repository.actionsFor("operator.session_created")).toHaveLength(1);
    expect(await service.authenticate(grant.token)).not.toBeNull();
  });

  it("rejects a wrong password with invalid_credentials and counts the failure", async () => {
    const { service, repository, operatorId, mailer } = await singleFactorHarness();

    await expect(
      service.loginPasswordOnly("ops@diogenesmodel.com", "not the password", context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });

    expect(mailer.outbox).toHaveLength(0);
    expect(repository.operators.get(operatorId)?.failedLoginCount).toBe(1);
    expect(repository.actionsFor("operator.login_rejected")[0]?.details).toMatchObject({
      reason: "password_mismatch",
    });
  });

  it("answers an unknown address with the same error, so accounts cannot be enumerated", async () => {
    const { service, mailer } = await singleFactorHarness();

    const wrongPassword = await service
      .loginPasswordOnly("ops@diogenesmodel.com", "wrong", context)
      .catch((thrown: ConsoleError) => thrown.code);
    const unknown = await service
      .loginPasswordOnly("nobody@diogenesmodel.com", password, context)
      .catch((thrown: ConsoleError) => thrown.code);

    expect(wrongPassword).toBe("invalid_credentials");
    expect(unknown).toBe("invalid_credentials");
    expect(mailer.outbox).toHaveLength(0);
  });

  it("refuses a suspended operator even with the right password", async () => {
    const { service, repository, operatorId } = await singleFactorHarness();
    await repository.setOperatorStatus(operatorId, "suspended");

    await expect(
      service.loginPasswordOnly("ops@diogenesmodel.com", password, context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
    expect(repository.actionsFor("operator.login_rejected").at(-1)?.details).toMatchObject({
      reason: "status_suspended",
    });
  });

  it("refuses a locked operator even with the right password", async () => {
    const { service, repository } = await singleFactorHarness();

    for (let attempt = 0; attempt < passwordOnlyConfig.limits.lockThreshold; attempt += 1) {
      await service
        .loginPasswordOnly("ops@diogenesmodel.com", "wrong", context)
        .catch(() => undefined);
    }

    await expect(
      service.loginPasswordOnly("ops@diogenesmodel.com", password, context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
    expect(repository.actionsFor("operator.login_rejected").at(-1)?.details).toMatchObject({
      reason: "locked",
    });
  });

  it("still delivers the reset link under password_only (the email path is unchanged)", async () => {
    const { service, mailer } = await singleFactorHarness();

    await service.requestPasswordReset("ops@diogenesmodel.com", context);

    expect(mailer.outbox).toHaveLength(1);
    expect(mailer.outbox[0]?.body).toMatch(/https?:\/\//); // a one-time link, not a login code
  });
});

describe("console login (cf_access)", () => {
  const cfConfig: ConsoleConfig = loadConfig({
    CONSOLE_ENV: "development",
    DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
    CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
    CONSOLE_IP_HASH_SALT: "b".repeat(48),
    CONSOLE_MAIL_MODE: "capture",
    CONSOLE_LOGIN_MODE: "cf_access",
    CONSOLE_CF_ACCESS_TEAM_DOMAIN: "hakkoai.cloudflareaccess.com",
    CONSOLE_CF_ACCESS_AUD: "a".repeat(64),
    CONSOLE_CF_ACCESS_ADMIN_EMAILS: "ops@diogenesmodel.com",
  });

  // A fake verifier stands in for the JWKS-backed one: the assertion string is a
  // label, not a real JWT (cf-access.test.ts covers the cryptographic verifier).
  const fakeVerifier = {
    async verify(assertion: string) {
      if (assertion === "good") return { email: "ops@diogenesmodel.com" };
      if (assertion === "stranger") return { email: "stranger@diogenesmodel.com" };
      throw new Error("invalid assertion");
    },
  };

  function cfHarness() {
    const repository = new MemoryConsoleRepository();
    const mailer = new MemoryMailer();
    const service = new AuthService(repository, mailer, cfConfig, fakeVerifier);
    return { repository, mailer, service };
  }

  it("provisions an admin operator on first Access sign-in and issues a session", async () => {
    const { service, repository } = cfHarness();

    const grant = await service.loginWithCfAccess("good", context);

    expect(grant.operator.email).toBe("ops@diogenesmodel.com");
    expect(grant.operator.role).toBe("admin");
    expect(grant.operator.status).toBe("active");
    expect(repository.actionsFor("operator.provisioned")).toHaveLength(1);
    expect(repository.actionsFor("operator.session_created")).toHaveLength(1);
    expect(await service.authenticate(grant.token)).not.toBeNull();
  });

  it("reuses the existing operator on later sign-ins", async () => {
    const { service, repository } = cfHarness();

    const first = await service.loginWithCfAccess("good", context);
    const second = await service.loginWithCfAccess("good", context);

    expect(second.operator.id).toBe(first.operator.id);
    expect(repository.operators.size).toBe(1);
    expect(repository.actionsFor("operator.provisioned")).toHaveLength(1); // provisioned exactly once
  });

  it("rejects an identity outside the console admin allowlist, provisioning nothing", async () => {
    const { service, repository } = cfHarness();

    await expect(service.loginWithCfAccess("stranger", context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
    expect(repository.actionsFor("operator.login_rejected").at(-1)?.details).toMatchObject({
      reason: "cf_access_not_allowed",
    });
    expect(repository.operators.size).toBe(0);
  });

  it("rejects a missing or invalid assertion", async () => {
    const { service } = cfHarness();

    await expect(service.loginWithCfAccess("", context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
    await expect(service.loginWithCfAccess("bogus", context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
  });

  // The regression that made every destructive write unreachable under this
  // mode: a JIT operator is stored with an unusable random hash, so sudo could
  // never be granted and purge / credential issuance / invite administration
  // were all permanently blocked for exactly the accounts cf_access creates.
  it("grants sudo by re-verifying the assertion, since there is no password to re-enter", async () => {
    const { service, repository } = cfHarness();
    const grant = await service.loginWithCfAccess("good", context);
    const session = (await service.authenticate(grant.token))!;

    await expect(
      service.grantSudo(session, { assertion: "good" }, context),
    ).resolves.toMatchObject({ sudoSeconds: cfConfig.session.sudoMinutes * 60 });
    // Recorded as what it is, so it is never mistaken for a password re-entry.
    expect(repository.actionsFor("operator.sudo_granted").at(-1)?.details).toMatchObject({
      factor: "cf_access_assertion",
    });
  });

  it("refuses sudo for a missing, invalid, or someone else's assertion", async () => {
    const { service } = cfHarness();
    const grant = await service.loginWithCfAccess("good", context);
    const session = (await service.authenticate(grant.token))!;

    for (const proof of [{}, { assertion: "bogus" }, { assertion: "stranger" }]) {
      await expect(service.grantSudo(session, proof, context)).rejects.toMatchObject({
        code: "invalid_credentials",
      });
    }
  });

  // Otherwise the decoy hash a JIT operator carries would be the thing being
  // checked, and a caller could pick the factor by choosing a field.
  it("ignores a password under cf_access rather than falling back to it", async () => {
    const { service } = cfHarness();
    const grant = await service.loginWithCfAccess("good", context);
    const session = (await service.authenticate(grant.token))!;

    await expect(
      service.grantSudo(session, { password: "anything at all" }, context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
  });
});

describe("verification codes", () => {
  it("spends one attempt per guess and stops at the configured budget", async () => {
    const { service, mailer } = await harness();
    const { challengeId } = await service.login("ops@diogenesmodel.com", password, context);
    const wrong = codeFor(mailer.lastCode());

    for (let attempt = 0; attempt < config.challenge.otpMaxAttempts; attempt += 1) {
      await expect(service.verifyOtp(challengeId, wrong, context)).rejects.toMatchObject({
        code: "invalid_credentials",
      });
    }

    // The budget is exhausted, so even the correct code is now refused.
    await expect(
      service.verifyOtp(challengeId, mailer.lastCode(), context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
  });

  it("accepts a code once", async () => {
    const value = await harness();
    const { challengeId } = await value.service.login("ops@diogenesmodel.com", password, context);
    const code = value.mailer.lastCode();

    await value.service.verifyOtp(challengeId, code, context);

    await expect(value.service.verifyOtp(challengeId, code, context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
  });

  it("expires a code after the configured window", async () => {
    const { service, repository, mailer } = await harness();
    const { challengeId } = await service.login("ops@diogenesmodel.com", password, context);
    const code = mailer.lastCode();

    repository.advance(config.challenge.otpTtlMinutes * 60_000 + 1_000);

    await expect(service.verifyOtp(challengeId, code, context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
  });

  it("invalidates the previous code on resend without spending an attempt", async () => {
    const { service, repository, mailer } = await harness();
    const first = await service.login("ops@diogenesmodel.com", password, context);
    const firstCode = mailer.lastCode();
    repository.advance(61_000);

    const second = await service.resendOtp(first.challengeId, context);
    const secondCode = mailer.lastCode();

    expect(second.challengeId).not.toBe(first.challengeId);
    expect(repository.challenges.get(first.challengeId)?.attemptCount).toBe(0);
    await expect(service.verifyOtp(first.challengeId, firstCode, context)).rejects.toMatchObject({
      code: "invalid_credentials",
    });
    await expect(service.verifyOtp(second.challengeId, secondCode, context)).resolves.toMatchObject({
      operator: { email: "ops@diogenesmodel.com" },
    });
  });

  it("answers a resend for an unknown challenge with the same shape", async () => {
    const { service, mailer } = await harness();

    const result = await service.resendOtp("00000000-0000-4000-8000-000000000000", context);

    expect(result.challengeId).toMatch(/^[0-9a-f-]{36}$/);
    expect(mailer.outbox).toHaveLength(0);
  });
});

describe("sessions", () => {
  it("rejects and revokes an idle session", async () => {
    const value = await harness();
    const grant = await signIn(value);

    value.repository.advance(config.session.idleMinutes * 60_000 + 1_000);

    expect(await value.service.authenticate(grant.token)).toBeNull();
    expect([...value.repository.sessions.values()][0]?.revokeReason).toBe("idle_timeout");
  });

  it("keeps a session alive while it is being used", async () => {
    const value = await harness();
    const grant = await signIn(value);

    for (let step = 0; step < 3; step += 1) {
      value.repository.advance(config.session.idleMinutes * 60_000 - 1_000);
      expect(await value.service.authenticate(grant.token)).not.toBeNull();
    }
  });

  it("expires a session at the absolute deadline even while in use", async () => {
    const value = await harness();
    const grant = await signIn(value);

    value.repository.advance(config.session.absoluteMinutes * 60_000 + 1_000);

    expect(await value.service.authenticate(grant.token)).toBeNull();
  });

  it("revokes every session of an operator that stops being active", async () => {
    const value = await harness();
    const grant = await signIn(value);
    await value.repository.setOperatorStatus(value.operatorId, "suspended");

    expect(await value.service.authenticate(grant.token)).toBeNull();
    expect([...value.repository.sessions.values()][0]?.revokeReason).toBe("operator_not_active");
  });

  it("drops the session on logout", async () => {
    const value = await harness();
    const grant = await signIn(value);
    const session = await value.service.authenticate(grant.token);

    await value.service.logout(session!, context);

    expect(await value.service.authenticate(grant.token)).toBeNull();
  });

  it("grants sudo only with the current password", async () => {
    const value = await harness();
    const grant = await signIn(value);
    const session = (await value.service.authenticate(grant.token))!;

    await expect(
      value.service.grantSudo(session, { password: "wrong" }, context),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
    await expect(
      value.service.grantSudo(session, { password }, context),
    ).resolves.toMatchObject({ sudoSeconds: config.session.sudoMinutes * 60 });
    expect(value.repository.actionsFor("operator.sudo_denied")).toHaveLength(1);
    expect(value.repository.actionsFor("operator.sudo_granted")).toHaveLength(1);
  });
});

describe("password lifecycle", () => {
  it("does not disclose whether a reset address exists", async () => {
    const { service, mailer } = await harness();

    await service.requestPasswordReset("nobody@diogenesmodel.com", context);

    expect(mailer.outbox).toHaveLength(0);
  });

  it("resets the password and revokes every existing session", async () => {
    const value = await harness();
    const grant = await signIn(value);
    value.repository.advance(61_000);

    await value.service.requestPasswordReset("ops@diogenesmodel.com", context);
    const link = value.mailer.lastLinkParams();
    await value.service.completeChallengePasswordSet(
      "password_reset",
      link.challenge,
      link.token,
      "a completely different phrase",
      context,
    );

    expect(await value.service.authenticate(grant.token)).toBeNull();
    const reset = value.repository.actionsFor("operator.password_reset_completed")[0];
    expect(reset?.details).toMatchObject({ revokedSessions: 1 });
  });

  it("refuses a reset token that does not match the challenge", async () => {
    const value = await harness();
    await value.service.requestPasswordReset("ops@diogenesmodel.com", context);
    const link = value.mailer.lastLinkParams();

    await expect(
      value.service.completeChallengePasswordSet(
        "password_reset",
        link.challenge,
        "z".repeat(43),
        "a completely different phrase",
        context,
      ),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
  });

  it("refuses a reset token replayed as an invite", async () => {
    const value = await harness();
    await value.service.requestPasswordReset("ops@diogenesmodel.com", context);
    const link = value.mailer.lastLinkParams();

    await expect(
      value.service.completeChallengePasswordSet(
        "invite",
        link.challenge,
        link.token,
        "a completely different phrase",
        context,
      ),
    ).rejects.toMatchObject({ code: "invalid_credentials" });
  });

  it("rejects a password that contains the account address", async () => {
    const value = await harness();
    await value.service.requestPasswordReset("ops@diogenesmodel.com", context);
    const link = value.mailer.lastLinkParams();

    await expect(
      value.service.completeChallengePasswordSet(
        "password_reset",
        link.challenge,
        link.token,
        "ops@diogenesmodel.com-2026",
        context,
      ),
    ).rejects.toMatchObject({ code: "weak_password" });
  });

  it("rejects a password below the minimum length", async () => {
    const value = await harness();
    await value.service.requestPasswordReset("ops@diogenesmodel.com", context);
    const link = value.mailer.lastLinkParams();

    await expect(
      value.service.completeChallengePasswordSet(
        "password_reset",
        link.challenge,
        link.token,
        "short",
        context,
      ),
    ).rejects.toMatchObject({ code: "weak_password" });
  });
});

describe("operator administration", () => {
  it("invites, activates, and signs in a new operator", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);

    const invited = await value.service.inviteOperator(
      admin,
      "qa@diogenesmodel.com",
      "viewer",
      context,
    );
    const link = value.mailer.lastLinkParams();
    await value.service.completeChallengePasswordSet(
      "invite",
      link.challenge,
      link.token,
      "another sufficiently long phrase",
      context,
    );

    expect(invited.status).toBe("invited");
    expect(value.repository.operators.get(invited.id)?.status).toBe("active");
  });

  it("refuses invitation and role changes from a non-admin", async () => {
    const value = await harness();
    const grant = await signIn(value);
    const session = (await value.service.authenticate(grant.token))!;

    await expect(
      value.service.inviteOperator(session, "qa@diogenesmodel.com", "viewer", context),
    ).rejects.toMatchObject({ code: "insufficient_role" });
    await expect(
      value.service.updateOperator(session, value.operatorId, { role: "admin" }, context),
    ).rejects.toMatchObject({ code: "insufficient_role" });
  });

  it("refuses a duplicate invitation", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);

    await expect(
      value.service.inviteOperator(admin, "ops@diogenesmodel.com", "viewer", context),
    ).rejects.toMatchObject({ code: "operator_exists" });
  });

  // A pending invite that expires used to brick the address: this method 409'd,
  // bootstrap-operator exited, password reset returns silently for a non-active
  // operator, and DELETE is refused by a trigger. Re-issuing is safe precisely
  // because the row was never activated.
  it("re-issues a pending invite and retires the previous link", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);

    const first = await value.service.inviteOperator(
      admin,
      "qa@diogenesmodel.com",
      "viewer",
      context,
    );
    const stale = value.mailer.lastLinkParams();

    const again = await value.service.inviteOperator(
      admin,
      "qa@diogenesmodel.com",
      "viewer",
      context,
    );
    const fresh = value.mailer.lastLinkParams();

    // Same operator, not a second row.
    expect(again.id).toBe(first.id);
    expect(fresh.challenge).not.toBe(stale.challenge);

    // The superseded link must not still set a password.
    await expect(
      value.service.completeChallengePasswordSet(
        "invite",
        stale.challenge,
        stale.token,
        "a sufficiently long replacement phrase",
        context,
      ),
    ).rejects.toBeDefined();

    // The new one works.
    await value.service.completeChallengePasswordSet(
      "invite",
      fresh.challenge,
      fresh.token,
      "a sufficiently long replacement phrase",
      context,
    );
    expect(value.repository.operators.get(first.id)?.status).toBe("active");
  });

  it("refuses to re-issue a pending invite under a different role", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);

    await value.service.inviteOperator(admin, "qa@diogenesmodel.com", "viewer", context);

    await expect(
      value.service.inviteOperator(admin, "qa@diogenesmodel.com", "admin", context),
    ).rejects.toMatchObject({ code: "operator_role_mismatch" });
  });

  // The security boundary of the re-issue path: an active operator must never be
  // re-invitable, or invitation becomes a password reset that skips proving
  // control of the mailbox.
  it("still refuses to re-invite an operator that has been activated", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);

    await value.service.inviteOperator(admin, "qa@diogenesmodel.com", "viewer", context);
    const link = value.mailer.lastLinkParams();
    await value.service.completeChallengePasswordSet(
      "invite",
      link.challenge,
      link.token,
      "a sufficiently long replacement phrase",
      context,
    );

    await expect(
      value.service.inviteOperator(admin, "qa@diogenesmodel.com", "viewer", context),
    ).rejects.toMatchObject({ code: "operator_exists" });
  });

  it("revokes sessions when a role or status changes and blocks self modification", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);
    const target = await value.repository.createOperator({
      email: "qa@diogenesmodel.com",
      role: "viewer",
      createdBy: admin.operator.id,
    });
    await value.repository.activateOperatorWithPassword(target.id, passwordHash);
    await value.repository.issueSession({
      operatorId: target.id,
      tokenHash: "d".repeat(64),
      absoluteSeconds: 3_600,
      ipHash: null,
      userAgent: null,
    });

    await value.service.updateOperator(admin, target.id, { role: "operator" }, context);

    expect(await value.repository.findSessionByTokenHash("d".repeat(64), 3_600)).toMatchObject({
      outcome: "missing",
    });
    await expect(
      value.service.updateOperator(admin, admin.operator.id, { status: "suspended" }, context),
    ).rejects.toMatchObject({ code: "self_modification" });
  });

  it("refuses to activate an operator that has not accepted the invitation", async () => {
    const value = await harness();
    const admin = await signInAsElevatedAdmin(value);
    const invited = await value.repository.createOperator({
      email: "qa@diogenesmodel.com",
      role: "viewer",
      createdBy: admin.operator.id,
    });

    await expect(
      value.service.updateOperator(admin, invited.id, { status: "active" }, context),
    ).rejects.toMatchObject({ code: "invite_pending" });
  });
});
