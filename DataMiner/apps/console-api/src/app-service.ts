import { createHash } from "node:crypto";

import type { RequestContext } from "./auth.js";
import type { ConsoleConfig } from "./config.js";
import type {
  AppLoginInput,
  ChangePasswordInput,
  ClaimConfirmInput,
  EnrollDeviceInput,
  ConsentRenewalInput,
  PasswordResetConfirmInput,
  PasswordResetRequestInput,
  ProfileInput,
  SignupInput,
  SignupResendInput,
  SignupVerifyInput,
} from "./app-contracts.js";
import type {
  ChallengeRejectionReason,
  PostgresAppRepository,
  ProfileRow,
} from "./app-repository.js";
import {
  badRequest,
  forbidden,
  invalidCredentials,
  notFound,
  signupVerificationFailed,
  tooManyRequests,
} from "./errors.js";
import {
  annotatorAddressInUseMail,
  annotatorOtpMail,
  type ConsoleMail,
  type Mailer,
} from "./mailer.js";
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

export type SignupResult = {
  challengeId: string;
  expiresInSeconds: number;
};

export type SessionGrant = {
  token: string;
  expiresInSeconds: number;
};

export type EnrollResult = {
  deviceId: string;
  clientDeviceId: string;
  /** Returned exactly once. A repeated enroll of the same machine gets null. */
  token: string | null;
};

/** Terms the server is currently serving. Frozen per deployment, not per request. */
export type TermsSnapshot = {
  policyVersion: string;
  purpose: string;
  termsHash: string;
  /**
   * Whether consent rows stamped with this terms version authorize trainable
   * use (docs/49 E2). Read from deployment config, never from the request: the
   * question "does this document grant trainable use" is answered once per
   * terms version by the operator who deployed it, not per signup. Defaults to
   * false everywhere so a new terms version must re-make the decision.
   */
  allowsTrainableUse: boolean;
  /**
   * What this terms version authorizes (docs/108 §3.2), sorted and deduplicated
   * by `parseTermsScopes`. Same discipline as `allowsTrainableUse`: read from
   * deployment config, stamped at signing time, never backfilled.
   *
   * This is what makes "re-signing expands your authorization" expressible at
   * all. With only a policy version string, the most a client could say is that
   * two labels differ; with a set, it can say which capture the new document
   * adds.
   */
  scopes: string[];
};

/**
 * The annotator-facing service. Everything it touches goes through the app
 * repository, which has its own pool and role — this class must never reach for
 * the console repository, or the isolation in 11 §4.2 becomes decorative.
 */
/**
 * Structured operational log. Deliberately narrow: this surface must never log
 * an address, a code, or a token, so the shape only admits the fields that are
 * safe to emit.
 */
export type AppServiceLog = (
  fields: {
    reason: string;
    errorName?: string | undefined;
    operation?: string | undefined;
    requestId?: string | undefined;
    severity?: "WARNING" | "ERROR" | undefined;
  },
  message: string,
) => void;

export class AppService {
  readonly #repository: PostgresAppRepository;
  readonly #mailer: Mailer;
  readonly #config: ConsoleConfig;
  readonly #terms: TermsSnapshot;
  readonly #termsBody: string;
  readonly #log: AppServiceLog | undefined;
  #decoyPasswordHash: Promise<string> | null = null;

  constructor(
    repository: PostgresAppRepository,
    mailer: Mailer,
    config: ConsoleConfig,
    terms: TermsSnapshot & { body?: string },
    log?: AppServiceLog,
  ) {
    this.#repository = repository;
    this.#mailer = mailer;
    this.#config = config;
    this.#terms = {
      policyVersion: terms.policyVersion,
      purpose: terms.purpose,
      termsHash: terms.termsHash,
      allowsTrainableUse: terms.allowsTrainableUse,
      // Copied and normalised here so the rest of the class can treat scopes as
      // a set: a caller keeping a reference to the array it passed in cannot
      // change what later consents get stamped with, and ordering never leaks
      // into a comparison.
      scopes: [...new Set(terms.scopes)].sort(),
    };
    this.#termsBody = terms.body ?? "";
    this.#log = log;
  }

  /**
   * Signup. The response shape is identical whether the address was free or
   * already taken, and a challenge id comes back either way — otherwise this
   * endpoint is an address oracle.
   *
   * Order matters more than it looks. The invite is validated *first*, before
   * anything branches on the address, because the two rejections do not look
   * alike: an unusable code is a 403 and a taken address is a 202. Checking the
   * address first meant a made-up code separated registered addresses from
   * unregistered ones for free. Now probing costs a genuinely redeemable code —
   * the residual oracle 11 §12.2 already priced in — and the code still is not
   * spent, because only the transaction below redeems.
   */
  async signup(input: SignupInput, context: RequestContext): Promise<SignupResult> {
    await this.#limit(`signup_ip:${context.ipHash}`, 3_600, this.#config.limits.loginPerIpHour);
    await this.#limit(
      `signup_email:${this.#hashEmail(input.email)}`,
      3_600,
      this.#config.limits.loginPerEmailHour,
    );

    if (input.acceptedPolicyVersion !== this.#terms.policyVersion) {
      // The client was displaying stale terms. Recording consent against what
      // the server currently serves would be recording a lie.
      throw badRequest(
        "terms_out_of_date",
        "the displayed terms are out of date; reload and read the current version",
      );
    }
    this.#assertPasswordAcceptable(input.password, input.email);

    const expiresInSeconds = this.#config.challenge.otpTtlMinutes * 60;
    if (!(await this.#repository.inviteRedeemable(input.inviteCode))) {
      // Unknown, revoked, exhausted, or expired — deliberately one message, and
      // deliberately identical for a free and a taken address.
      throw forbidden("invite_invalid", "the invite code is invalid or has already been used");
    }

    const existing = await this.#repository.findCredentialByEmail(input.email);
    if (existing) {
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.signup_rejected",
        entityType: "annotator_email",
        entityId: this.#hashEmail(input.email),
        details: { reason: "address_in_use" },
      });
      // Same shape, same work, same mail, and in the same *order* — argon2 then
      // delivery, both awaited, exactly like the branch below. The hash is
      // thrown away; it is here so a taken address costs the argon2 pass a free
      // one does, since the cached decoy this used to await returned in
      // microseconds and timing answered what the response body would not.
      //
      // Running the two concurrently would undo most of that: the free path
      // cannot parallelise them (it needs the hash before it can insert), so
      // one branch would cost `max(argon2, mail)` and the other
      // `argon2 + mail`, a stable difference an attacker can measure. Serial
      // here leaves only the signup transaction between them.
      //
      // The notice mail is 11 §7.1's, and tells the real owner someone tried to
      // register with their address.
      await hashPassword(input.password);
      await this.#deliverBestEffort(input.email, annotatorAddressInUseMail(), {
        annotatorId: existing.annotator_id,
        reason: "address_in_use_notice",
      });
      return { challengeId: this.#deadChallengeId(), expiresInSeconds };
    }

    const passwordHash = await hashPassword(input.password);
    const created = await this.#repository.signup({
      code: input.inviteCode,
      email: input.email,
      passwordHash,
      alias: input.alias,
      profile: input.profile ?? null,
      consent: this.#terms,
      ipHash: context.ipHash,
      userAgent: context.userAgent,
    });
    if (!created) {
      // The gate above passed, so this is the narrow race where a concurrent
      // signup took the last use. Same message, for the same reason.
      throw forbidden("invite_invalid", "the invite code is invalid or has already been used");
    }

    const challengeId = await this.#issueEmailChallenge(
      created.annotatorId,
      "signup_verify",
      input.email,
      context,
    );
    return { challengeId, expiresInSeconds };
  }

  /**
   * Reissues a signup verification code.
   *
   * This closes the hole that made a failed first delivery terminal: the invite
   * was already spent and the account already created, so without a resend the
   * user could neither verify nor sign up again — the address was taken, by
   * themselves. A staging allowlist rejection or any provider outage put every
   * new account in that state.
   *
   * It keys on the challenge id rather than the address on purpose. The id came
   * back from signup and only the party that signed up holds it, so this is not
   * a second place to ask "is this address registered?" — an id that resolves to
   * nothing gets the same shape as one that resolves to a pending account.
   */
  async resendSignupCode(
    input: SignupResendInput,
    context: RequestContext,
  ): Promise<SignupResult> {
    // The caller's address first, because it is the only thing here the caller
    // does not choose. Keying solely on the challenge id — as the first cut did
    // — was no limit at all: an unknown id mints a fresh bucket, so unlimited
    // random ids meant unlimited rows in `console_rate_limits` from an
    // unauthenticated route.
    await this.#limit(`resend_ip:${context.ipHash}`, 3_600, this.#config.limits.otpSendPerIpHour);

    const expiresInSeconds = this.#config.challenge.otpTtlMinutes * 60;
    const pending = await this.#repository.findSignupChallengeForResend(input.challengeId);
    if (!pending) {
      // No row, already verified, or suspended. Nothing to send, nothing to say.
      return { challengeId: this.#deadChallengeId(), expiresInSeconds };
    }

    // Then the account, which is stable across reissues. Keying on the
    // challenge id let a caller walk the chain: each resend returns a *new* id,
    // and this lookup deliberately accepts superseded ones, so following the
    // returned id gave a fresh budget every time and the mail went out forever.
    await this.#limit(
      `resend:${pending.annotatorId}`,
      3_600,
      this.#config.limits.otpSendPerHour,
    );

    const challengeId = await this.#issueEmailChallenge(
      pending.annotatorId,
      "signup_verify",
      pending.email,
      context,
    );
    await this.#repository.recordAudit({
      actorType: "service",
      actorId: null,
      action: "annotator.signup_code_resent",
      entityType: "annotator",
      entityId: pending.annotatorId,
      details: { requestId: context.requestId },
    });
    return { challengeId, expiresInSeconds };
  }

  /**
   * Verification activates the account (D5 + D6): a verified address is the only
   * gate, and passing it means active — no operator approval step, because
   * issuing the invite was already the authorization decision.
   */
  async verifySignup(input: SignupVerifyInput, context: RequestContext): Promise<void> {
    const challenge = await this.#repository.claimChallengeAttempt(
      input.challengeId,
      this.#config.challenge.otpMaxAttempts,
    );
    if (!challenge || challenge.purpose !== "signup_verify") {
      const reason = challenge
        ? "challenge_wrong_purpose"
        : await this.#repository.classifyChallengeRejection(
            input.challengeId,
            "signup_verify",
            this.#config.challenge.otpMaxAttempts,
          );
      this.#rejectSignupVerification(reason, context);
    }

    if (!secretEquals(hashChallengeCode(challenge.codeSalt, input.code), challenge.codeHash)) {
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.verify_failed",
        entityType: "annotator",
        entityId: challenge.annotatorId,
        details: { attempt: challenge.attemptCount },
      });
      this.#rejectSignupVerification("code_mismatch", context);
    }
    if (!(await this.#repository.consumeChallenge(challenge.id))) {
      this.#rejectSignupVerification("consume_conflict", context);
    }
    if (!(await this.#repository.activateCredential(challenge.annotatorId))) {
      // Already active, or suspended in the meantime. Either way this code has
      // nothing left to do.
      this.#rejectSignupVerification("credential_not_pending", context);
    }

    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: challenge.annotatorId,
      action: "annotator.activated",
      entityType: "annotator",
      entityId: challenge.annotatorId,
      details: { requestId: context.requestId },
    });
  }

  #rejectSignupVerification(
    reason: ChallengeRejectionReason | "code_mismatch" | "consume_conflict" | "credential_not_pending",
    context: RequestContext,
  ): never {
    this.#log?.(
      {
        operation: "signup_verify",
        reason,
        requestId: context.requestId,
        severity: "WARNING",
      },
      "annotator signup verification rejected",
    );
    throw signupVerificationFailed();
  }

  /**
   * Start a password reset: mail a six digit code to an address that owns an
   * active account.
   *
   * Until this existed an annotator who forgot their password had no way back
   * in at all — there is no operator-side reset for annotator credentials
   * either, so the account was simply lost. The enum already carried
   * `password_reset`; only the flow was missing.
   *
   * The response is identical whether or not the address has an account, and it
   * always costs one challenge-shaped delay, because this route is
   * unauthenticated: a caller must not be able to use it to test which
   * addresses are registered. That is the same reason signup answers with a
   * dead challenge id rather than "already registered".
   */
  async requestPasswordReset(
    input: PasswordResetRequestInput,
    context: RequestContext,
  ): Promise<SignupResult> {
    // Address first: it is the one thing the caller does not choose.
    await this.#limit(`reset_ip:${context.ipHash}`, 3_600, this.#config.limits.otpSendPerIpHour);
    await this.#limit(
      `reset_email:${this.#hashEmail(input.email)}`,
      3_600,
      this.#config.limits.otpSendPerHour,
    );

    const expiresInSeconds = this.#config.challenge.otpTtlMinutes * 60;
    const credential = await this.#repository.findCredentialByEmail(input.email);
    if (
      !credential ||
      credential.status !== "active" ||
      credential.annotator_status !== "active"
    ) {
      // No account, not yet verified, or suspended. Same answer as success:
      // the shape of this response must not depend on any of that.
      return { challengeId: this.#deadChallengeId(), expiresInSeconds };
    }

    // One live reset code per account (`issueChallenge` supersedes open ones of
    // the same purpose): an old code still working would widen the window for
    // one that leaked from a mailbox.
    const challengeId = await this.#issueEmailChallenge(
      credential.annotator_id,
      "password_reset",
      credential.email,
      context,
    );
    await this.#repository.recordAudit({
      actorType: "service",
      actorId: null,
      action: "annotator.password_reset_requested",
      entityType: "annotator",
      entityId: credential.annotator_id,
      details: { requestId: context.requestId },
    });
    return { challengeId, expiresInSeconds };
  }

  /**
   * Finish a password reset: the code proves control of the mailbox, so it buys
   * exactly one new password.
   *
   * Every live session is revoked afterwards. Someone resetting may be doing it
   * because the old password leaked, and a session token minted with that
   * password would otherwise keep working. Device credentials are left alone —
   * see `revokeAllSessionsForAnnotatorSql`.
   */
  async confirmPasswordReset(
    input: PasswordResetConfirmInput,
    context: RequestContext,
  ): Promise<void> {
    const challenge = await this.#repository.claimChallengeAttempt(
      input.challengeId,
      this.#config.challenge.otpMaxAttempts,
    );
    if (!challenge || challenge.purpose !== "password_reset") throw invalidCredentials();

    if (!secretEquals(hashChallengeCode(challenge.codeSalt, input.code), challenge.codeHash)) {
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.password_reset_failed",
        entityType: "annotator",
        entityId: challenge.annotatorId,
        details: { attempt: challenge.attemptCount },
      });
      throw invalidCredentials();
    }
    const credential = await this.#repository.findCredentialByAnnotatorId(challenge.annotatorId);
    if (credential) this.#assertPasswordAcceptable(input.password, credential.email);

    // Consume before the write, so a replayed request cannot set a second
    // password with the same code.
    if (!(await this.#repository.consumeChallenge(challenge.id))) throw invalidCredentials();

    const passwordHash = await hashPassword(input.password);
    if (!(await this.#repository.resetCredentialPassword(challenge.annotatorId, passwordHash))) {
      // Suspended between issuing the code and using it.
      throw invalidCredentials();
    }
    await this.#repository.revokeAllSessionsForAnnotator(challenge.annotatorId, "password_reset");

    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: challenge.annotatorId,
      action: "annotator.password_reset",
      entityType: "annotator",
      entityId: challenge.annotatorId,
      details: { requestId: context.requestId },
    });
  }

  /**
   * Login returns a session token directly. Unlike the operator flow there is no
   * mandatory per-login email code: the operator surface can sign credentials
   * and trigger purges, an annotator can only upload data its own device
   * produced, and a code on every launch would make the desktop client unusable
   * (11 §7.3).
   */
  async login(input: AppLoginInput, context: RequestContext): Promise<SessionGrant> {
    await this.#limit(`login_ip:${context.ipHash}`, 3_600, this.#config.limits.loginPerIpHour);
    await this.#limit(
      `login_email:${this.#hashEmail(input.email)}`,
      3_600,
      this.#config.limits.loginPerEmailHour,
    );

    const credential = await this.#repository.findCredentialByEmail(input.email);
    // Verified even when the row is missing or the account is suspended, so an
    // unknown address costs the same wall clock as a wrong password.
    const matches = credential
      ? await verifyPassword(credential.password_hash, input.password)
      : await this.#verifyDecoy(input.password);

    const locked =
      credential?.locked_until !== null &&
      credential?.locked_until !== undefined &&
      new Date(credential.locked_until).getTime() > Date.now();

    if (
      !credential ||
      !matches ||
      locked ||
      credential.status !== "active" ||
      credential.annotator_status !== "active"
    ) {
      if (credential && !matches) {
        await this.#repository.registerLoginFailure(credential.annotator_id, {
          threshold: this.#config.limits.lockThreshold,
          baseMinutes: this.#config.limits.lockBaseMinutes,
          maxMinutes: this.#config.limits.lockMaxMinutes,
        });
      }
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.login_rejected",
        entityType: credential ? "annotator" : "annotator_email",
        entityId: credential?.annotator_id ?? this.#hashEmail(input.email),
        details: { reason: this.#rejectionReason(credential, matches, locked) },
      });
      throw invalidCredentials();
    }

    await this.#repository.clearLoginFailures(credential.annotator_id);
    const grant = await this.#issueSession(credential.annotator_id, context);
    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: credential.annotator_id,
      action: "annotator.session_created",
      entityType: "annotator",
      entityId: credential.annotator_id,
      details: {},
    });
    return grant;
  }

  async authenticate(token: string) {
    return this.#repository.findSession(hashSessionToken(token), {
      windowSeconds: this.sessionWindowSeconds,
    });
  }

  /** The sliding window (docs/34 §6.1a), for cookie Max-Age and login replies. */
  get sessionWindowSeconds(): number {
    return this.#config.appSession.slidingDays * 86_400;
  }

  async logout(token: string): Promise<void> {
    await this.#repository.revokeSession(hashSessionToken(token), "logout");
  }

  /**
   * Password change for a signed-in caller (docs/34 §6.3). The current
   * password is re-verified — a session alone must not be enough to change the
   * secret that mints sessions — and a wrong guess climbs the same lockout
   * ladder as a failed login, so a stolen session cannot brute-force the old
   * password at leisure. Every *other* session is revoked; the one that just
   * proved itself keeps working. Device credentials are untouched, as with
   * reset.
   */
  async changePassword(
    annotatorId: string,
    tokenHash: string,
    input: ChangePasswordInput,
    context: RequestContext,
  ): Promise<void> {
    const credential = await this.#repository.findCredentialByAnnotatorId(annotatorId);
    if (!credential) throw invalidCredentials();

    const locked =
      credential.locked_until !== null &&
      new Date(credential.locked_until).getTime() > Date.now();
    const matches = await verifyPassword(credential.password_hash, input.currentPassword);
    if (!matches || locked) {
      if (!matches) {
        await this.#repository.registerLoginFailure(annotatorId, {
          threshold: this.#config.limits.lockThreshold,
          baseMinutes: this.#config.limits.lockBaseMinutes,
          maxMinutes: this.#config.limits.lockMaxMinutes,
        });
      }
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.password_change_rejected",
        entityType: "annotator",
        entityId: annotatorId,
        details: { reason: locked ? "locked" : "password_mismatch" },
      });
      throw invalidCredentials();
    }

    this.#assertPasswordAcceptable(input.newPassword, credential.email);
    const passwordHash = await hashPassword(input.newPassword);
    if (!(await this.#repository.resetCredentialPassword(annotatorId, passwordHash))) {
      throw invalidCredentials();
    }
    await this.#repository.revokeOtherSessionsForAnnotator(
      annotatorId,
      tokenHash,
      "password_changed",
    );

    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: annotatorId,
      action: "annotator.password_changed",
      entityType: "annotator",
      entityId: annotatorId,
      details: { requestId: context.requestId },
    });
  }

  async getProfile(annotatorId: string): Promise<ProfileRow | null> {
    return this.#repository.getProfile(annotatorId);
  }

  /**
   * The consent block the account surface renders.
   *
   * `matchesCurrentTerms` is the whole point: a consent is captured once at
   * signup and was, until this existed, frozen forever — there was no way for
   * someone to sign an updated document, and no way for them to even see that
   * theirs had fallen behind. Comparing the recorded `termsHash` against what
   * the deployment currently serves is what makes that visible.
   *
   * `allowsTrainableUse` is reported from the stored row, not from current
   * config: it records what was true when they signed.
   */
  async getConsentState(annotatorId: string): Promise<{
    consent: {
      consentId: string;
      policyVersion: string;
      termsHash: string;
      allowsTrainableUse: boolean;
      scopes: string[];
      acceptedAt: string | null;
    } | null;
    currentPolicyVersion: string;
    currentTermsHash: string;
    currentAllowsTrainableUse: boolean;
    currentScopes: string[];
    matchesCurrentTerms: boolean;
    pendingScopes: string[];
    resignRequired: boolean;
  }> {
    const consent = await this.#repository.currentConsent(annotatorId);
    // Hash, not version string: the same version label with different bytes is
    // a different document, and that is exactly the drift worth catching.
    const matchesCurrentTerms = consent?.termsHash === this.#terms.termsHash;
    const signedScopes = consent?.scopes ?? [];
    return {
      consent: consent
        ? {
            consentId: consent.id,
            policyVersion: consent.policyVersion,
            termsHash: consent.termsHash,
            allowsTrainableUse: consent.allowsTrainableUse,
            scopes: consent.scopes,
            acceptedAt: consent.acceptedAt,
          }
        : null,
      currentPolicyVersion: this.#terms.policyVersion,
      currentTermsHash: this.#terms.termsHash,
      currentAllowsTrainableUse: this.#terms.allowsTrainableUse,
      currentScopes: this.#terms.scopes,
      matchesCurrentTerms,
      // The answer to "what would re-signing get you". Reported as a difference
      // rather than as two version labels because that is the only form anybody
      // can act on: nobody re-reads two legal documents to diff them, and until
      // this existed nothing in the system could name the delta.
      //
      // A difference in the other direction — the current terms authorize less
      // than what somebody already signed — is deliberately not reported here.
      // Signing a narrower document is a real thing an operator can deploy, but
      // it is not something the annotator can fix by clicking a button, so it
      // does not belong on a prompt that asks them to click one.
      pendingScopes: this.#terms.scopes.filter((scope) => !signedScopes.includes(scope)),
      resignRequired: !matchesCurrentTerms,
    };
  }

  /**
   * Sign the terms the deployment currently serves.
   *
   * This is the way back for someone who registered under an older document —
   * or under the same document when the deployment answered a different way
   * about trainable use. Without it the only remedies were rewriting a consent
   * row by hand (which relabels data already captured under it) or abandoning
   * the account and registering a second one.
   *
   * Already-current is a no-op rather than an error: re-signing the identical
   * document should not pile up rows, and the caller should not have to check
   * first.
   */
  async renewConsent(
    input: ConsentRenewalInput,
    annotatorId: string,
    context: RequestContext,
  ): Promise<{ consentId: string | null; renewed: boolean }> {
    // Same guard as signup: a client displaying stale terms must not be able to
    // record consent against what the server currently serves.
    if (input.acceptedPolicyVersion !== this.#terms.policyVersion) {
      throw badRequest(
        "terms_out_of_date",
        "the displayed terms are out of date; reload and read the current version",
      );
    }
    const existing = await this.#repository.currentConsent(annotatorId);
    if (existing?.termsHash === this.#terms.termsHash) {
      return { consentId: existing.id, renewed: false };
    }

    const consentId = await this.#repository.recordConsentRenewal(annotatorId, {
      policyVersion: this.#terms.policyVersion,
      purpose: this.#terms.purpose,
      termsHash: this.#terms.termsHash,
      allowsTrainableUse: this.#terms.allowsTrainableUse,
      scopes: this.#terms.scopes,
    });
    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: annotatorId,
      action: "annotator.consent_renewed",
      entityType: "consent",
      entityId: consentId,
      details: {
        policyVersion: this.#terms.policyVersion,
        termsHash: this.#terms.termsHash,
        scopes: this.#terms.scopes,
        // What this re-signature actually added. Recorded rather than derived
        // later because the previous consent's scopes are the only evidence of
        // what the annotator had already agreed to at this moment, and a future
        // terms change would make the same subtraction give a different answer.
        addedScopes: this.#terms.scopes.filter(
          (scope) => !(existing?.scopes ?? []).includes(scope),
        ),
        supersededConsentId: existing?.id ?? null,
        requestId: context.requestId,
      },
    });
    return { consentId, renewed: true };
  }

  async updateProfile(
    annotatorId: string,
    profile: ProfileInput,
    context: RequestContext,
  ): Promise<ProfileRow | null> {
    await this.#repository.upsertProfile(annotatorId, profile);
    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: annotatorId,
      action: "annotator.profile_updated",
      entityType: "annotator",
      entityId: annotatorId,
      // Field names only, never values: audit rows survive purge, so a value
      // written here would be contact data nothing can ever remove (docs/34
      // §5.4).
      details: { fields: Object.keys(profile).sort(), requestId: context.requestId },
    });
    return this.#repository.getProfile(annotatorId);
  }

  /**
   * What the claim landing page renders before the user types anything
   * (docs/34 §6.5): the activation branch shows terms, the reset branch does
   * not. Read-only — previewing a link must not spend its attempt budget.
   * Unknown, spent, superseded, and expired all answer the same 404; the id
   * space is 122 random bits, so this is not an enumeration surface.
   */
  async claimPreflight(
    challengeId: string,
    context: RequestContext,
  ): Promise<{ kind: "activate" | "reset"; email: string; terms?: TermsSnapshot & { body: string } }> {
    await this.#limit(`claim_ip:${context.ipHash}`, 3_600, this.#config.limits.loginPerIpHour);
    const claim = await this.#repository.findClaimChallenge(challengeId);
    if (!claim || claim.credentialStatus === "suspended") {
      throw notFound("claim_not_found", "the link is invalid, expired, or already used");
    }
    const kind = claim.credentialStatus === "pending_verification" ? "activate" : "reset";
    return {
      kind,
      email: maskEmail(claim.email),
      ...(kind === "activate" ? { terms: this.terms() } : {}),
    };
  }

  /**
   * Redeems an operator-issued claim link (docs/34 §5.2/§6.5). Which flow this
   * is — first activation or a password reset — is decided here from the
   * credential status, never by the caller: a link is a capability to set a
   * password for one account, not a menu.
   */
  async confirmClaim(input: ClaimConfirmInput, context: RequestContext): Promise<void> {
    await this.#limit(`claim_ip:${context.ipHash}`, 3_600, this.#config.limits.loginPerIpHour);

    const challenge = await this.#repository.claimChallengeAttempt(
      input.challengeId,
      this.#config.challenge.otpMaxAttempts,
    );
    if (!challenge || challenge.purpose !== "account_claim") throw invalidCredentials();
    if (!secretEquals(hashChallengeCode(challenge.codeSalt, input.token), challenge.codeHash)) {
      await this.#repository.recordAudit({
        actorType: "service",
        actorId: null,
        action: "annotator.claim_failed",
        entityType: "annotator",
        entityId: challenge.annotatorId,
        details: { attempt: challenge.attemptCount },
      });
      throw invalidCredentials();
    }

    const credential = await this.#repository.findCredentialByAnnotatorId(challenge.annotatorId);
    if (!credential || credential.annotator_status !== "active") throw invalidCredentials();
    this.#assertPasswordAcceptable(input.password, credential.email);

    if (credential.status === "pending_verification") {
      // Activation: the user is reading the terms on this page, so the version
      // check is the same one signup applies — recording consent against text
      // the user did not see would be recording a lie.
      if (input.acceptedPolicyVersion !== this.#terms.policyVersion) {
        throw badRequest(
          "terms_out_of_date",
          "the displayed terms are out of date; reload and read the current version",
        );
      }
      if (!(await this.#repository.consumeChallenge(challenge.id))) throw invalidCredentials();
      const passwordHash = await hashPassword(input.password);
      if (
        !(await this.#repository.claimActivate({
          annotatorId: challenge.annotatorId,
          passwordHash,
          consent: this.#terms,
        }))
      ) {
        throw invalidCredentials();
      }
      if (input.profile) {
        await this.#repository.upsertProfile(challenge.annotatorId, input.profile);
      }
      await this.#repository.recordAudit({
        actorType: "annotator",
        actorId: challenge.annotatorId,
        action: "annotator.claimed",
        entityType: "annotator",
        entityId: challenge.annotatorId,
        details: { requestId: context.requestId },
      });
      return;
    }

    if (credential.status !== "active") throw invalidCredentials();
    // Reset: consent stands, terms are not re-signed, and every session dies —
    // an operator triggers this exactly because the old secret may be in the
    // wrong hands.
    if (!(await this.#repository.consumeChallenge(challenge.id))) throw invalidCredentials();
    const passwordHash = await hashPassword(input.password);
    if (!(await this.#repository.resetCredentialPassword(challenge.annotatorId, passwordHash))) {
      throw invalidCredentials();
    }
    await this.#repository.revokeAllSessionsForAnnotator(challenge.annotatorId, "password_reset");
    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: challenge.annotatorId,
      action: "annotator.password_reset",
      entityType: "annotator",
      entityId: challenge.annotatorId,
      details: { requestId: context.requestId, via: "operator_link" },
    });
  }

  /**
   * Device enrollment. The credential is minted here and returned once; the
   * database keeps only its sha256, which is exactly what the ingest gate
   * matches against (see the ingest `authenticateDevice`).
   */
  async enrollDevice(
    annotatorId: string,
    input: EnrollDeviceInput,
    context: RequestContext,
  ): Promise<EnrollResult> {
    // A rate, not a cap. Devices are unbounded per annotator by decision (D4);
    // what this stops is a stolen session enrolling a fleet in one burst
    // (11 §12.5).
    await this.#limit(`enroll:${annotatorId}`, 3_600, this.#config.limits.loginPerEmailHour);

    const token = generateOpaqueToken();
    const device = await this.#repository.enrollDevice({
      annotatorId,
      clientDeviceId: input.clientDeviceId,
      credentialFingerprint: hashSessionToken(token),
      windowsBuild: input.windowsBuild,
      gpuPciId: input.gpuPciId,
      gpuDriver: input.gpuDriver,
      displaySnapshot: input.displaySnapshot,
      inputDevicesSnapshot: input.inputDevicesSnapshot,
      clientVersion: input.clientVersion,
    });

    if (!device.created) {
      if (!device.id) {
        // The machine id belongs to someone else. Same message as a malformed
        // request rather than confirming that it exists.
        throw forbidden("device_unavailable", "this device cannot be enrolled by this account");
      }
      // Replay keeps the machine's existing credential. Handing out a second
      // token would leave two live credentials with no way to tell which one
      // the client retained.
      return { deviceId: device.id, clientDeviceId: device.clientDeviceId, token: null };
    }
    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: annotatorId,
      action: "device.enrolled",
      entityType: "device",
      entityId: device.id,
      details: {
        clientDeviceId: input.clientDeviceId,
        requestId: context.requestId,
      },
    });
    return { deviceId: device.id, clientDeviceId: device.clientDeviceId, token };
  }

  async listDevices(annotatorId: string) {
    return this.#repository.listDevices(annotatorId);
  }

  async listConsents(annotatorId: string) {
    return this.#repository.listConsents(annotatorId);
  }

  /**
   * `/app/v1/session` 的身份摘要（docs/65 J-45）：登录后的页面要能确认「我是谁」。
   * 邮箱不打码——收件人就是本人，登录用的就是这个地址；对他人展示时的打码规则
   * （maskEmail）不适用于自视图。
   */
  async sessionSummary(annotatorId: string) {
    const [annotatorRow, credential] = await Promise.all([
      this.#repository.findAnnotator(annotatorId),
      this.#repository.findCredentialByAnnotatorId(annotatorId),
    ]);
    return {
      annotatorId,
      alias: annotatorRow?.alias ?? null,
      status: annotatorRow?.status ?? "unknown",
      email: credential?.email ?? null,
    };
  }

  /**
   * 绑定前的签名 catalog 启动（docs/37 §Bootstrap）。
   *
   * 为什么必须存在：设备绑定要提交一个**服务端已批准**的设备配置身份，而客户端
   * 只能从签名 catalog 里知道有哪些；可签名 catalog 的下发（`GET /v1/catalog`）
   * 又要求设备凭据，而设备凭据只有绑定成功后才签发。一台新机器因此永远进不来
   * ——这就是 #323 的启动循环。
   *
   * 为什么这样开口是安全的：它返回的字节与 `/v1/catalog` **逐字节相同**，是那条
   * 不可变的已发布 release。一个持有标注员会话的调用方本来就可以绑定设备、拿到
   * 设备凭据、再用它拉同一份 catalog；所以这条路没有多给出任何东西，只是把那三
   * 步里客户端唯一真正需要的一步提前。它不签发凭据、不写任何行、不接受任何客户
   * 端提交的目录内容。
   *
   * 客户端侧的信任判断一条都没挪：pinned 公钥验签、逐 profile content_hash、
   * 版本单调防回滚、缓存重验，全部照旧在客户端发生（docs/16 §2 I1–I7）。
   */
  async bootstrapCatalog(annotatorId: string, channel: "dev") {
    // 与其它标注员接口同一套限流。这条路只读且不昂贵，限流是为了让一个被盗会话
    // 没法把它当成低成本的目录轮询口。
    await this.#limit(`catalog-bootstrap:${annotatorId}`, 3_600, 120);
    return this.#repository.currentCatalogRelease(channel);
  }

  /**
   * The terms the client must display and confirm before signup.
   *
   * Without this the surface was unusable from a real client: signup refuses a
   * mismatched `acceptedPolicyVersion` (§7.1), and nothing told the client what
   * the current version *is* — so the only way to get it right was to compile it
   * into the binary, which means every published client breaks the day the terms
   * change. Serving it also means consent is given against text the user could
   * actually read; the hash is verified against these bytes at startup.
   */
  terms(): TermsSnapshot & { body: string } {
    return { ...this.#terms, body: this.#termsBody };
  }

  /**
   * Re-issues a device credential to its owner.
   *
   * This is the way out of a dead end. Enrollment is idempotent per machine, so
   * a second enroll of an already-registered `clientDeviceId` returns the device
   * with `token: null` — correct, because minting a second live credential for
   * one machine leaves nobody able to say which one the client kept. But a
   * client that lost its credential (reinstall, a wiped credential store, a new
   * Windows user) then had no way to get one: re-enrolling replays, and the
   * operator-side rotation lives behind the governance surface it cannot reach.
   *
   * The ownership predicate is in the statement, not here, so a caller cannot
   * rotate a device it does not own by guessing an id.
   *
   * The old credential dies immediately — 11 §10 accepted that, and it is why
   * this is a deliberate action rather than something the client does on a
   * whim: an upload in flight will start failing with 401 until it retries with
   * the new one.
   */
  async rotateDeviceCredential(
    annotatorId: string,
    deviceId: string,
    context: RequestContext,
  ): Promise<{ deviceId: string; token: string }> {
    await this.#limit(`rotate:${annotatorId}`, 3_600, this.#config.limits.otpSendPerHour);

    const token = generateOpaqueToken();
    const rotated = await this.#repository.rotateDeviceCredential(
      deviceId,
      annotatorId,
      hashSessionToken(token),
    );
    if (!rotated) {
      // Unknown, retired, or someone else's — one message, so this is not a
      // probe for which device ids exist.
      throw forbidden("device_unavailable", "this device cannot be rotated by this account");
    }

    await this.#repository.recordAudit({
      actorType: "annotator",
      actorId: annotatorId,
      action: "device.credential_rotated",
      entityType: "device",
      entityId: deviceId,
      details: { requestId: context.requestId, by: "annotator" },
    });
    return { deviceId, token };
  }

  /**
   * Ingests a telemetry batch authenticated by the *device* credential, not the
   * user session: the reporter runs unattended alongside capture, long after any
   * interactive sign-in has expired.
   */
  async recordClientEvents(
    credentialFingerprint: string,
    events: ReadonlyArray<{
      eventId: string;
      occurredAt: string;
      kind: string;
      severity: string;
      clientVersion: string;
      sessionId?: string | undefined;
      details: Record<string, unknown>;
    }>,
  ): Promise<{ accepted: number }> {
    const deviceId = await this.#repository.authenticateDevice(credentialFingerprint);
    if (!deviceId) throw invalidCredentials();

    for (const event of events) {
      await this.#repository.recordClientEvent({
        eventId: event.eventId,
        deviceId,
        sessionId: event.sessionId ?? null,
        occurredAt: event.occurredAt,
        kind: event.kind,
        severity: event.severity,
        clientVersion: event.clientVersion,
        details: event.details,
      });
    }
    return { accepted: events.length };
  }

  async #issueSession(annotatorId: string, context: RequestContext): Promise<SessionGrant> {
    const token = generateOpaqueToken();
    // The sliding window, not the operator absolute TTL (docs/34 §6.1a): an
    // annotator session lives until it is revoked or goes unused for a whole
    // window. `findSession` pushes expiry back out on use.
    const windowSeconds = this.sessionWindowSeconds;
    await this.#repository.issueSession({
      annotatorId,
      tokenHash: hashSessionToken(token),
      absoluteSeconds: windowSeconds,
      ipHash: context.ipHash,
      userAgent: context.userAgent,
    });
    return { token, expiresInSeconds: windowSeconds };
  }

  async #issueEmailChallenge(
    annotatorId: string,
    purpose: "signup_verify" | "password_reset",
    email: string,
    context: RequestContext,
  ): Promise<string> {
    const code = generateOtpCode();
    const salt = generateSalt();
    const challengeId = await this.#repository.issueChallenge({
      annotatorId,
      purpose,
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, code),
      ttlSeconds: this.#config.challenge.otpTtlMinutes * 60,
    });
    await this.#deliverBestEffort(
      email,
      annotatorOtpMail(code, this.#config.challenge.otpTtlMinutes),
      { annotatorId, reason: purpose, requestId: context.requestId },
    );
    return challengeId;
  }

  /**
   * Mail is sent outside the signup transaction, so by the time it runs the
   * invite is spent and the account exists. A delivery failure therefore must
   * not propagate: raising here turned a provider outage — or a staging
   * allowlist that legitimately refuses end-user addresses — into a 500 on a
   * request that had already succeeded, burning the code and stranding the
   * account with no way to verify it and no way to sign up again.
   *
   * The failure is recorded instead, and the caller returns the same challenge
   * id it would have on success. The user's route out is `signup/resend`.
   *
   * Two signals, not one. The log line is first and is the one to alert on: a
   * dead mail provider and a struggling database are the same incident often
   * enough that an audit row alone can be lost precisely when it matters. The
   * audit row is the durable record. Writing it is itself best-effort — the
   * account is already committed, and there is nothing left worth failing for.
   */
  async #deliverBestEffort(
    email: string,
    mail: Omit<ConsoleMail, "to">,
    audit: { annotatorId: string; reason: string; requestId?: string },
  ): Promise<void> {
    try {
      await this.#mailer.send({ to: email, ...mail });
    } catch (error) {
      const errorName = error instanceof Error ? error.name : "unknown";
      this.#log?.(
        { reason: audit.reason, errorName, requestId: audit.requestId },
        "annotator mail delivery failed",
      );
      try {
        await this.#repository.recordAudit({
          actorType: "service",
          actorId: null,
          action: "annotator.mail_delivery_failed",
          entityType: "annotator",
          entityId: audit.annotatorId,
          details: {
            reason: audit.reason,
            // The class name only — a provider error can carry the address, and
            // this row is not the place for it.
            error: errorName,
            ...(audit.requestId ? { requestId: audit.requestId } : {}),
          },
        });
      } catch {
        // The account is already committed. Nothing left worth failing for.
      }
    }
  }

  async #limit(bucket: string, windowSeconds: number, limit: number): Promise<void> {
    const verdict = await this.#repository.consumeRateLimit(bucket, windowSeconds, limit);
    if (!verdict.allowed) {
      throw tooManyRequests("too many attempts; try again later", verdict.retryAfterSeconds);
    }
  }

  // Buckets and audit ids carry a hashed address, never a raw one — the same
  // salt the console面 uses for the equivalent purpose.
  #hashEmail(email: string): string {
    return hashSubject(this.#config.ipHashSalt, email);
  }

  /**
   * A syntactically valid id that matches no row. The caller gets the same
   * response shape as a real signup and learns nothing from it.
   */
  #deadChallengeId(): string {
    const digest = createHash("sha256").update(generateOpaqueToken()).digest("hex");
    return [
      digest.slice(0, 8),
      digest.slice(8, 12),
      `4${digest.slice(13, 16)}`,
      `8${digest.slice(17, 20)}`,
      digest.slice(20, 32),
    ].join("-");
  }

  async #decoy(): Promise<string> {
    this.#decoyPasswordHash ??= createDecoyPasswordHash();
    return this.#decoyPasswordHash;
  }

  async #verifyDecoy(password: string): Promise<boolean> {
    await verifyPassword(await this.#decoy(), password);
    return false;
  }

  /**
   * The same "must not contain the address" rule the operator side enforces
   * (docs/34 A9 closed the asymmetry). Length stays zod's job — the schema
   * already refuses short passwords at the boundary.
   */
  #assertPasswordAcceptable(password: string, email: string): void {
    const normalised = password.toLowerCase();
    const address = email.toLowerCase();
    const localPart = address.slice(0, address.indexOf("@"));
    if (normalised.includes(address) || (localPart.length >= 4 && normalised.includes(localPart))) {
      throw badRequest("weak_password", "password must not contain the account address");
    }
  }

  #rejectionReason(
    credential: { status: string; annotator_status: string } | null,
    matches: boolean,
    locked: boolean,
  ): string {
    if (!credential) return "unknown_address";
    if (!matches) return "password_mismatch";
    if (locked) return "locked";
    if (credential.status === "pending_verification") return "email_unverified";
    if (credential.status === "suspended") return "credential_suspended";
    if (credential.annotator_status !== "active") return "annotator_suspended";
    return "rejected";
  }
}

/**
 * Enough for the link holder to recognise their own address, not enough for a
 * shoulder-surfer to learn it: first character of the local part plus the
 * domain.
 */
export function maskEmail(email: string): string {
  const at = email.indexOf("@");
  if (at <= 0) return "***";
  return `${email[0]}***${email.slice(at)}`;
}
