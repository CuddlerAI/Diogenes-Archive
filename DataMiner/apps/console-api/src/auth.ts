import { randomUUID } from "node:crypto";

import type { CfAccessIdentityVerifier } from "./cf-access.js";
import type { ConsoleConfig } from "./config.js";
import type {
  AuthenticatedSession,
  ChallengePurpose,
  ConsoleRepository,
  OperatorRecord,
  OperatorRole,
} from "./domain.js";
import { badRequest, conflict, forbidden, invalidCredentials, notFound, tooManyRequests } from "./errors.js";
import { inviteMail, otpMail, passwordResetMail, RecipientNotAllowedError, type Mailer } from "./mailer.js";
import { requireCapability } from "./rbac.js";
import {
  createDecoyPasswordHash,
  generateOpaqueToken,
  generateOtpCode,
  generateSalt,
  hashChallengeCode,
  hashPassword,
  hashSessionToken,
  hashSubject,
  secretEquals,
  verifyPassword,
} from "./secrets.js";

export type RequestContext = {
  requestId: string;
  ipHash: string;
  userAgent: string | null;
};

export type LoginResult = {
  challengeId: string;
  expiresInSeconds: number;
};

export type SessionGrant = {
  token: string;
  maxAgeSeconds: number;
  operator: OperatorRecord;
};

export class AuthService {
  readonly #repository: ConsoleRepository;
  readonly #mailer: Mailer;
  readonly #config: ConsoleConfig;
  readonly #cfAccess: CfAccessIdentityVerifier | null;
  #decoyPasswordHash: Promise<string> | null = null;

  constructor(
    repository: ConsoleRepository,
    mailer: Mailer,
    config: ConsoleConfig,
    cfAccessVerifier: CfAccessIdentityVerifier | null = null,
  ) {
    this.#repository = repository;
    this.#mailer = mailer;
    this.#config = config;
    this.#cfAccess = cfAccessVerifier;
  }

  async login(email: string, password: string, context: RequestContext): Promise<LoginResult> {
    const operator = await this.#authenticatePassword(email, password, context);
    const expiresInSeconds = this.#config.challenge.otpTtlMinutes * 60;
    if (!operator) {
      // A rejected attempt still returns a challenge shape so the caller cannot
      // tell an unknown address from a wrong password. The identifier belongs to
      // no row, so verification can never succeed.
      return { challengeId: randomUUID(), expiresInSeconds };
    }

    await this.#enforceOtpSendLimits(operator.email, context);
    const challengeId = await this.#issueOtpChallenge(operator, context);
    return { challengeId, expiresInSeconds };
  }

  // Single-factor sign-in (docs/20 §6.A, password_only). It shares every
  // password, rate-limit, lockout, and audit rule with the two-step path — only
  // the second factor is gone. A rejection throws the same invalid_credentials
  // for an unknown address, a wrong password, a suspended account, and a locked
  // one, so it leaks no more about account existence than the OTP path does. The
  // email OTP still guards invite and password reset, where it proves control of
  // the mailbox.
  async loginPasswordOnly(
    email: string,
    password: string,
    context: RequestContext,
  ): Promise<SessionGrant> {
    const operator = await this.#authenticatePassword(email, password, context);
    if (!operator) throw invalidCredentials();

    const grant = await this.#issueSession(operator, context);
    await this.#repository.markOperatorLoggedIn(operator.id);
    await this.#audit(context, {
      actorType: "operator",
      actorId: operator.id,
      action: "operator.session_created",
      entityType: "operator",
      entityId: operator.id,
      details: { factor: "password_only" },
    });
    return grant;
  }

  // Cloudflare Access single sign-on (docs/20 §6.D, cf_access mode). The console
  // asks for no password of its own: it verifies the signed Access assertion,
  // checks the email against a console-side admin allowlist (a second gate over
  // the Access policy itself), and issues a session — provisioning the operator
  // on first sight so an approved administrator is never blocked on an invite.
  async loginWithCfAccess(assertion: string, context: RequestContext): Promise<SessionGrant> {
    if (!this.#cfAccess || !this.#config.cfAccess) throw invalidCredentials();

    let email: string;
    try {
      email = (await this.#cfAccess.verify(assertion)).email;
    } catch {
      // A missing or invalid assertion collapses to the one credential error,
      // like every other rejected sign-in.
      throw invalidCredentials();
    }

    if (!this.#config.cfAccess.adminEmails.includes(email)) {
      await this.#audit(context, {
        actorType: "service",
        actorId: null,
        action: "operator.login_rejected",
        entityType: "operator_email",
        entityId: this.#hashEmail(email),
        details: { reason: "cf_access_not_allowed" },
      });
      throw invalidCredentials();
    }

    let operator = await this.#repository.findOperatorByEmail(email);
    if (!operator) {
      // `active` requires a password hash at the database level, so an unusable
      // random one is stored: cf_access has no password path, and this keeps the
      // invariant intact without a schema change.
      const created = await this.#repository.createOperator({ email, role: "admin", createdBy: null });
      await this.#repository.activateOperatorWithPassword(created.id, await createDecoyPasswordHash());
      operator = await this.#repository.findOperatorById(created.id);
      await this.#audit(context, {
        actorType: "service",
        actorId: null,
        action: "operator.provisioned",
        entityType: "operator",
        entityId: created.id,
        details: { via: "cf_access", role: "admin" },
      });
    }
    if (!operator || operator.status !== "active") throw invalidCredentials();

    const grant = await this.#issueSession(operator, context);
    await this.#repository.markOperatorLoggedIn(operator.id);
    await this.#audit(context, {
      actorType: "operator",
      actorId: operator.id,
      action: "operator.session_created",
      entityType: "operator",
      entityId: operator.id,
      details: { factor: "cf_access" },
    });
    return grant;
  }

  // Spends the attempt limits, verifies the password (against a decoy hash for an
  // unknown address so timing cannot separate the two), and on rejection both
  // registers the failure toward lockout and audits it. Returns the active
  // operator on success with its failure count cleared, or null on any rejection.
  // Each caller layers its own second step — an OTP challenge or a session — on
  // top, so the shared security rules live in exactly one place.
  async #authenticatePassword(
    email: string,
    password: string,
    context: RequestContext,
  ): Promise<OperatorRecord | null> {
    // Attempt limits are spent on every call, so an unknown address and a wrong
    // password consume exactly the same budget. The mail-sending limits are
    // spent by the OTP path only once a code is actually about to be sent, so a
    // wrong password cannot exhaust a real operator's daily code allowance.
    await this.#enforceLimit(`login_ip:${context.ipHash}`, 3_600, this.#config.limits.loginPerIpHour);
    await this.#enforceLimit(
      `login_email:${this.#hashEmail(email)}`,
      3_600,
      this.#config.limits.loginPerEmailHour,
    );

    const operator = await this.#repository.findOperatorByEmail(email);
    const passwordMatches = await this.#verifyAgainst(operator, password);

    if (!operator || !passwordMatches || operator.status !== "active" || operator.locked) {
      if (operator && !passwordMatches) {
        await this.#repository.registerLoginFailure(operator.id, {
          threshold: this.#config.limits.lockThreshold,
          baseMinutes: this.#config.limits.lockBaseMinutes,
          maxMinutes: this.#config.limits.lockMaxMinutes,
        });
      }
      await this.#audit(context, {
        actorType: "service",
        actorId: null,
        action: "operator.login_rejected",
        entityType: operator ? "operator" : "operator_email",
        entityId: operator?.id ?? this.#hashEmail(email),
        details: { reason: this.#rejectionReason(operator, passwordMatches) },
      });
      return null;
    }

    await this.#repository.clearLoginFailures(operator.id);
    return operator;
  }

  async verifyOtp(challengeId: string, code: string, context: RequestContext): Promise<SessionGrant> {
    const challenge = await this.#repository.claimChallengeAttempt(
      challengeId,
      this.#config.challenge.otpMaxAttempts,
    );
    if (!challenge || challenge.purpose !== "login_otp") throw invalidCredentials();

    if (!secretEquals(hashChallengeCode(challenge.codeSalt, code), challenge.codeHash)) {
      await this.#audit(context, {
        actorType: "service",
        actorId: null,
        action: "operator.otp_failed",
        entityType: "operator",
        entityId: challenge.operatorId,
        details: { attempt: challenge.attemptCount },
      });
      throw invalidCredentials();
    }
    if (!(await this.#repository.consumeChallenge(challenge.id))) throw invalidCredentials();

    const operator = await this.#repository.findOperatorById(challenge.operatorId);
    if (!operator || operator.status !== "active") throw invalidCredentials();

    const grant = await this.#issueSession(operator, context);
    await this.#repository.markOperatorLoggedIn(operator.id);
    await this.#audit(context, {
      actorType: "operator",
      actorId: operator.id,
      action: "operator.session_created",
      entityType: "operator",
      entityId: operator.id,
      details: {},
    });
    return grant;
  }

  async resendOtp(challengeId: string, context: RequestContext): Promise<LoginResult> {
    const expiresInSeconds = this.#config.challenge.otpTtlMinutes * 60;
    await this.#enforceLimit(
      `otp_resend_ip:${context.ipHash}`,
      3_600,
      this.#config.limits.otpSendPerIpHour,
    );

    // Reading the challenge must not spend a verification attempt, so this path
    // deliberately avoids claimChallengeAttempt.
    const challenge = await this.#repository.findOpenChallenge(challengeId);
    const operator =
      challenge && challenge.purpose === "login_otp"
        ? await this.#repository.findOperatorById(challenge.operatorId)
        : null;
    if (!operator || operator.status !== "active") {
      return { challengeId: randomUUID(), expiresInSeconds };
    }

    await this.#enforceOtpSendLimits(operator.email, context);
    // Superseding inside issue invalidates the previous code, so a resent
    // message always replaces rather than adds a usable code.
    return { challengeId: await this.#issueOtpChallenge(operator, context), expiresInSeconds };
  }

  async authenticate(token: string): Promise<AuthenticatedSession | null> {
    const found = await this.#repository.findSessionByTokenHash(
      hashSessionToken(token),
      this.#config.session.idleMinutes * 60,
    );
    if (found.outcome === "missing") return null;
    if (found.outcome === "idle") {
      await this.#repository.revokeSession(found.sessionId, "idle_timeout");
      return null;
    }

    if (found.operator.status !== "active") {
      await this.#repository.revokeOperatorSessions(found.operator.id, "operator_not_active");
      return null;
    }

    await this.#repository.touchSession(found.session.id);
    return { session: found.session, operator: found.operator };
  }

  async logout(session: AuthenticatedSession, context: RequestContext): Promise<void> {
    await this.#repository.revokeSession(session.session.id, "logout");
    await this.#audit(context, {
      actorType: "operator",
      actorId: session.operator.id,
      action: "operator.session_revoked",
      entityType: "operator_session",
      entityId: session.session.id,
      details: { reason: "logout" },
    });
  }

  /**
   * Step-up for destructive writes: purge, credential issuance, grant
   * revocation, invite administration.
   *
   * The factor follows the login mode, because under `cf_access` there is no
   * password to re-enter. A JIT-provisioned operator is stored with an
   * unusable random hash (there is no password path, and `active` requires a
   * hash at the database level), so verifying against it could never succeed —
   * every sudo-gated write was permanently unreachable for exactly the accounts
   * cf_access creates. The only way through was to send yourself a password
   * reset, which contradicts the mode's own premise.
   *
   * Under `cf_access` the assertion is therefore re-verified instead. Be honest
   * about what that buys: the browser resends the same edge-injected token it
   * already had, so this confirms "you are still inside a live Access session"
   * rather than "you just proved who you are". It is weaker than re-entering a
   * password, and the audit records which factor was used so the two are never
   * conflated after the fact. It is still not nothing — the assertion is a
   * signed token bound to this application's AUD, so a stolen console cookie
   * alone does not carry it.
   */
  async grantSudo(
    session: AuthenticatedSession,
    proof: { password?: string | undefined; assertion?: string | undefined },
    context: RequestContext,
  ): Promise<{ sudoSeconds: number }> {
    await this.#enforceLimit(`sudo:${session.operator.id}`, 3_600, 20);
    const factor = this.#config.loginMode === "cf_access" ? "cf_access_assertion" : "password";
    const proven =
      factor === "cf_access_assertion"
        ? await this.#assertionMatchesOperator(session.operator, proof.assertion)
        : await this.#verifyAgainst(session.operator, proof.password ?? "");
    if (!proven) {
      await this.#audit(context, {
        actorType: "operator",
        actorId: session.operator.id,
        action: "operator.sudo_denied",
        entityType: "operator",
        entityId: session.operator.id,
        details: { factor },
      });
      throw invalidCredentials();
    }
    const sudoSeconds = this.#config.session.sudoMinutes * 60;
    await this.#repository.grantSessionSudo(session.session.id, sudoSeconds);
    await this.#audit(context, {
      actorType: "operator",
      actorId: session.operator.id,
      action: "operator.sudo_granted",
      entityType: "operator_session",
      entityId: session.session.id,
      details: { sudoSeconds, factor },
    });
    return { sudoSeconds };
  }

  /**
   * The assertion has to belong to the operator whose session is asking. Without
   * the address comparison any allow-listed colleague's assertion would elevate
   * anyone's session, which is the one thing this check exists to prevent.
   */
  async #assertionMatchesOperator(
    operator: OperatorRecord,
    assertion: string | undefined,
  ): Promise<boolean> {
    if (!this.#cfAccess || !assertion) return false;
    try {
      const claims = await this.#cfAccess.verify(assertion);
      return secretEquals(claims.email, operator.email);
    } catch {
      return false;
    }
  }

  async requestPasswordReset(email: string, context: RequestContext): Promise<void> {
    await this.#enforceLimit(
      `reset_ip:${context.ipHash}`,
      3_600,
      this.#config.limits.otpSendPerIpHour,
    );
    await this.#enforceOtpSendLimits(email, context);

    const operator = await this.#repository.findOperatorByEmail(email);
    if (!operator || operator.status !== "active") return;

    await this.#repository.supersedeOpenChallenges(operator.id, "password_reset");
    const token = generateOpaqueToken();
    const salt = generateSalt();
    const challenge = await this.#repository.issueChallenge({
      operatorId: operator.id,
      purpose: "password_reset",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, token),
      ttlSeconds: this.#config.challenge.passwordResetTtlMinutes * 60,
    });
    await this.#deliver(
      operator.email,
      passwordResetMail(
        this.#link("reset-password", challenge.id, token),
        this.#config.challenge.passwordResetTtlMinutes,
      ),
      context,
      operator.id,
      "operator.password_reset_requested",
    );
  }

  async completeChallengePasswordSet(
    purpose: Extract<ChallengePurpose, "invite" | "password_reset">,
    challengeId: string,
    token: string,
    password: string,
    context: RequestContext,
  ): Promise<void> {
    const challenge = await this.#repository.claimChallengeAttempt(challengeId, 5);
    if (!challenge || challenge.purpose !== purpose) throw invalidCredentials();
    if (!secretEquals(hashChallengeCode(challenge.codeSalt, token), challenge.codeHash)) {
      throw invalidCredentials();
    }

    const operator = await this.#repository.findOperatorById(challenge.operatorId);
    if (!operator) throw invalidCredentials();
    const expectedStatus = purpose === "invite" ? "invited" : "active";
    if (operator.status !== expectedStatus) throw invalidCredentials();
    this.#assertPasswordAcceptable(password, operator.email);

    if (!(await this.#repository.consumeChallenge(challenge.id))) throw invalidCredentials();

    const passwordHash = await hashPassword(password);
    if (purpose === "invite") {
      await this.#repository.activateOperatorWithPassword(operator.id, passwordHash);
    } else {
      await this.#repository.setOperatorPassword(operator.id, passwordHash);
    }
    // A password change invalidates every session, including the one that may
    // have requested the reset.
    const revoked = await this.#repository.revokeOperatorSessions(operator.id, "password_changed");
    await this.#audit(context, {
      actorType: "operator",
      actorId: operator.id,
      action: purpose === "invite" ? "operator.invite_accepted" : "operator.password_reset_completed",
      entityType: "operator",
      entityId: operator.id,
      details: { revokedSessions: revoked },
    });
  }

  async inviteOperator(
    actor: AuthenticatedSession,
    email: string,
    role: OperatorRole,
    context: RequestContext,
  ): Promise<OperatorRecord> {
    this.#guardOperatorAdmin(actor);
    const existing = await this.#repository.findOperatorByEmail(email);
    // Re-inviting is allowed for exactly one status: `invited`. Such a row has
    // never been activated — no password, no sessions — so issuing a fresh
    // invite cannot take anything over, and without this an operator whose
    // invite link was lost or expired is stuck forever: bootstrap-operator
    // exits on an existing row, this method used to 409, password reset returns
    // silently for a non-active operator, and DELETE is refused by a trigger.
    // The only escape was hand-written SQL against the console database.
    //
    // It stays a 409 for `active`, `suspended` and `tombstoned`. Re-inviting an
    // active operator would be a password reset that skips proving control of
    // the mailbox, which is a different and much stronger capability than
    // "finish setting up an account that was never used".
    if (existing && existing.status !== "invited") {
      throw conflict("operator_exists", "an operator with this address already exists");
    }
    if (existing) {
      // Re-inviting is "send this pending person a working link again", not a
      // way to change what they will become. Silently keeping the stored role
      // would hand back a record that contradicts the request, and silently
      // applying the new one would make this a privilege change on a path whose
      // guards were written for creation. Say so instead; PATCH is the way to
      // change a role.
      if (existing.role !== role) {
        throw conflict(
          "operator_role_mismatch",
          "a pending invite for this address exists with a different role; change the role first, then re-invite",
        );
      }
      // The old link must stop working the moment a new one is issued.
      await this.#repository.supersedeOpenChallenges(existing.id, "invite");
    }

    const operator =
      existing ??
      (await this.#repository.createOperator({
        email,
        role,
        createdBy: actor.operator.id,
      }));
    const token = generateOpaqueToken();
    const salt = generateSalt();
    const challenge = await this.#repository.issueChallenge({
      operatorId: operator.id,
      purpose: "invite",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, token),
      ttlSeconds: this.#config.challenge.inviteTtlHours * 3_600,
    });
    await this.#deliver(
      operator.email,
      inviteMail(this.#link("accept-invite", challenge.id, token), this.#config.challenge.inviteTtlHours),
      context,
      actor.operator.id,
      "operator.invited",
      // `reissued` separates "a new operator now exists" from "an existing
      // pending invite was replaced", which are different facts to read back
      // out of the audit trail.
      { operatorId: operator.id, role, reissued: existing !== null },
    );
    return operator;
  }

  async updateOperator(
    actor: AuthenticatedSession,
    operatorId: string,
    changes: { role?: OperatorRole | undefined; status?: "active" | "suspended" | undefined },
    context: RequestContext,
  ): Promise<OperatorRecord> {
    this.#guardOperatorAdmin(actor);
    const target = await this.#repository.findOperatorById(operatorId);
    if (!target) throw notFound("operator_not_found", "operator was not found");
    if (target.id === actor.operator.id) {
      throw forbidden("self_modification", "an admin cannot change their own role or status");
    }
    if (changes.status === "active" && target.status === "invited") {
      throw conflict("invite_pending", "the invitation must be accepted before activation");
    }

    let updated = target;
    if (changes.role && changes.role !== target.role) {
      updated = await this.#repository.setOperatorRole(operatorId, changes.role);
      await this.#repository.revokeOperatorSessions(operatorId, "role_changed");
      await this.#audit(context, {
        actorType: "operator",
        actorId: actor.operator.id,
        action: "operator.role_changed",
        entityType: "operator",
        entityId: operatorId,
        details: { from: target.role, to: changes.role },
      });
    }
    if (changes.status && changes.status !== updated.status) {
      updated = await this.#repository.setOperatorStatus(operatorId, changes.status);
      if (changes.status === "suspended") {
        await this.#repository.revokeOperatorSessions(operatorId, "suspended");
      }
      await this.#audit(context, {
        actorType: "operator",
        actorId: actor.operator.id,
        action: "operator.status_changed",
        entityType: "operator",
        entityId: operatorId,
        details: { from: target.status, to: changes.status },
      });
    }
    return updated;
  }

  async listOperators(actor: AuthenticatedSession): Promise<OperatorRecord[]> {
    // Reading the roster is not a step-up action; only changing it is.
    requireCapability(actor.operator.role, "operator.admin");
    return this.#repository.listOperators();
  }

  /**
   * Administering operators creates and revokes governance access itself, so it
   * belongs behind the same step-up as purge and credential issuance — it was
   * the one write of comparable blast radius sitting outside it. A hijacked
   * admin session could otherwise mint a second admin, or lock the real ones
   * out, for the whole idle window without re-proving anything.
   */
  #guardOperatorAdmin(actor: AuthenticatedSession): void {
    requireCapability(actor.operator.role, "operator.admin");
    if (!actor.session.sudoActive) {
      throw forbidden("sudo_required", "confirm this action to continue");
    }
  }

  async #issueOtpChallenge(operator: OperatorRecord, context: RequestContext): Promise<string> {
    await this.#repository.supersedeOpenChallenges(operator.id, "login_otp");
    const code = generateOtpCode();
    const salt = generateSalt();
    const challenge = await this.#repository.issueChallenge({
      operatorId: operator.id,
      purpose: "login_otp",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, code),
      ttlSeconds: this.#config.challenge.otpTtlMinutes * 60,
    });
    await this.#deliver(
      operator.email,
      otpMail(code, this.#config.challenge.otpTtlMinutes),
      context,
      operator.id,
      "operator.otp_sent",
    );
    return challenge.id;
  }

  async #issueSession(operator: OperatorRecord, context: RequestContext): Promise<SessionGrant> {
    const token = generateOpaqueToken();
    const maxAgeSeconds = this.#config.session.absoluteMinutes * 60;
    await this.#repository.issueSession({
      operatorId: operator.id,
      tokenHash: hashSessionToken(token),
      absoluteSeconds: maxAgeSeconds,
      ipHash: context.ipHash,
      userAgent: context.userAgent,
    });
    return { token, maxAgeSeconds, operator };
  }

  async #deliver(
    recipient: string,
    mail: { subject: string; body: string; html?: string },
    context: RequestContext,
    actorId: string,
    action: string,
    details: Record<string, unknown> = {},
  ): Promise<void> {
    try {
      await this.#mailer.send({ to: recipient, ...mail });
    } catch (error) {
      if (error instanceof RecipientNotAllowedError) {
        await this.#audit(context, {
          actorType: "service",
          actorId: null,
          action: "operator.mail_blocked",
          entityType: "operator",
          entityId: actorId,
          details: { reason: "allowlist" },
        });
        throw forbidden("recipient_not_allowed", "this environment only delivers mail to allowlisted recipients");
      }
      throw error;
    }
    await this.#audit(context, {
      actorType: "operator",
      actorId,
      action,
      entityType: "operator",
      entityId: actorId,
      details,
    });
  }

  async #verifyAgainst(operator: OperatorRecord | null, password: string): Promise<boolean> {
    const storedHash = operator?.passwordHash ?? (await this.#decoyHash());
    return verifyPassword(storedHash, password);
  }

  async #decoyHash(): Promise<string> {
    this.#decoyPasswordHash ??= createDecoyPasswordHash();
    return this.#decoyPasswordHash;
  }

  #rejectionReason(operator: OperatorRecord | null, passwordMatches: boolean): string {
    if (!operator) return "unknown_operator";
    if (operator.locked) return "locked";
    if (!passwordMatches) return "password_mismatch";
    return `status_${operator.status}`;
  }

  #assertPasswordAcceptable(password: string, email: string): void {
    if (password.length < this.#config.password.minLength) {
      throw badRequest(
        "weak_password",
        `password must be at least ${this.#config.password.minLength} characters`,
      );
    }
    const normalised = password.toLowerCase();
    const localPart = email.slice(0, email.indexOf("@"));
    if (normalised.includes(email.toLowerCase()) || (localPart.length >= 4 && normalised.includes(localPart))) {
      throw badRequest("weak_password", "password must not contain the account address");
    }
  }

  #hashEmail(email: string): string {
    return hashSubject(this.#config.ipHashSalt, email);
  }

  #link(route: string, challengeId: string, token: string): string {
    const url = new URL(`/${route}`, this.#config.publicOrigin);
    url.searchParams.set("challenge", challengeId);
    url.searchParams.set("token", token);
    return url.toString();
  }

  async #enforceOtpSendLimits(email: string, context: RequestContext): Promise<void> {
    const subject = this.#hashEmail(email);
    await this.#enforceLimit(`otp_min:${subject}`, 60, this.#config.limits.otpSendPerMinute);
    await this.#enforceLimit(`otp_hour:${subject}`, 3_600, this.#config.limits.otpSendPerHour);
    await this.#enforceLimit(`otp_day:${subject}`, 86_400, this.#config.limits.otpSendPerDay);
    await this.#enforceLimit(
      `otp_ip:${context.ipHash}`,
      3_600,
      this.#config.limits.otpSendPerIpHour,
    );
  }

  async #enforceLimit(bucket: string, windowSeconds: number, limit: number): Promise<void> {
    const verdict = await this.#repository.consumeRateLimit(bucket, windowSeconds, limit);
    if (!verdict.allowed) {
      throw tooManyRequests("too many attempts, retry later", verdict.retryAfterSeconds);
    }
  }

  async #audit(
    context: RequestContext,
    event: {
      actorType: "operator" | "service";
      actorId: string | null;
      action: string;
      entityType: string;
      entityId: string;
      details: Record<string, unknown>;
    },
  ): Promise<void> {
    await this.#repository.recordAudit({
      ...event,
      requestId: context.requestId,
      details: { ...event.details, ipHash: context.ipHash },
    });
  }
}
