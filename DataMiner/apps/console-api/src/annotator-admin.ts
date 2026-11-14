import type { AnnotatorRegistrationInput, ProfileInput } from "./app-contracts.js";
import type { PostgresAppRepository, ProfileRow } from "./app-repository.js";
import type { RequestContext } from "./auth.js";
import type { ConsoleConfig } from "./config.js";
import type { AuthenticatedSession, ConsoleRepository } from "./domain.js";
import { conflict, forbidden, notFound } from "./errors.js";
import { annotatorClaimMail, annotatorResetLinkMail, type ConsoleMail, type Mailer } from "./mailer.js";
import { requireCapability } from "./rbac.js";
import {
  createDecoyPasswordHash,
  generateOpaqueToken,
  generateSalt,
  hashChallengeCode,
} from "./secrets.js";

export type ClaimIssued = {
  annotatorId: string;
  /**
   * The full claim URL, returned exactly once — the same discipline as device
   * tokens and invite plaintext. It exists in the response so an operator can
   * hand it over when the mailbox is unreachable; when they do, the audit row
   * already says `delivery: manual`.
   */
  claimUrl: string;
  expiresInSeconds: number;
};

/**
 * Operator-side administration of annotator credentials (docs/34 §5.2/§6.5).
 *
 * The seam this class exists for: handlers live on the governance surface
 * (proxy gate, operator session, RBAC, sudo), but every credential write runs
 * through the injected app repository — the `dataminer_app` pool and role. The
 * governance role keeps zero write access to annotator credentials, so a bug
 * here cannot become a governance-side credential write; that isolation is the
 * ratified verdict of docs/34 §7.8.
 *
 * What is *never* here: a password, a temporary password, or any path where an
 * operator learns a credential. The operator mints a one-shot claim link; the
 * annotator sets the password (docs/34 §7.4, hard boundary).
 */
export class AnnotatorAdminService {
  readonly #repository: PostgresAppRepository;
  readonly #auditSink: ConsoleRepository;
  readonly #mailer: Mailer;
  readonly #config: ConsoleConfig;

  constructor(
    repository: PostgresAppRepository,
    auditSink: ConsoleRepository,
    mailer: Mailer,
    config: ConsoleConfig,
  ) {
    this.#repository = repository;
    this.#auditSink = auditSink;
    this.#mailer = mailer;
    this.#config = config;
  }

  /**
   * One-step registration (docs/34 §5.2): annotator row, optional profile,
   * shadow credential, claim link. No invite code — the sudo-gated action is
   * itself the authorization decision, and burning a code on top of it would
   * make the same decision twice (11 §12.3 discipline).
   */
  async register(
    actor: AuthenticatedSession,
    input: AnnotatorRegistrationInput,
    context: RequestContext,
  ): Promise<ClaimIssued & { alias: string }> {
    this.#guard(actor);

    // This surface is authenticated and operators can already search by email,
    // so unlike the public endpoints a taken address is a plain conflict, not
    // something to blind (docs/34 §5.2). The unique index still backs this up
    // against races inside the transaction.
    const existing = await this.#repository.findCredentialByEmail(input.email);
    if (existing) {
      throw conflict("email_in_use", "an annotator with this address already exists");
    }

    const created = await this.#repository.registerByOperator({
      email: input.email,
      alias: input.alias,
      decoyPasswordHash: await createDecoyPasswordHash(),
      profile: input.profile ?? null,
    });
    if (!created) {
      throw conflict("email_in_use", "an annotator with this address already exists");
    }

    const issued = await this.#issueClaim(
      created.annotatorId,
      this.#config.challenge.inviteTtlHours * 3_600,
    );
    await this.#audit(actor, context, "annotator.credential_invited", created.annotatorId, {
      alias: input.alias,
      ...(input.note ? { note: input.note } : {}),
      reissued: false,
      delivery: "mail",
    });
    await this.#deliverBestEffort(
      input.email,
      annotatorClaimMail(issued.claimUrl, this.#config.challenge.inviteTtlHours),
      { annotatorId: created.annotatorId, reason: "account_claim" },
    );
    return { ...issued, alias: input.alias };
  }

  /**
   * Issues (or re-issues) a claim link for an existing annotator: the
   * governance-created shell that never had a credential, or a pending account
   * whose link expired. An active credential is refused — that is what the
   * reset below is for, and the two must not blur: activation records consent,
   * reset does not.
   */
  async inviteCredential(
    actor: AuthenticatedSession,
    annotatorId: string,
    email: string,
    context: RequestContext,
  ): Promise<ClaimIssued> {
    this.#guard(actor);

    const annotator = await this.#repository.findAnnotator(annotatorId);
    if (!annotator) throw notFound("annotator_not_found", "the annotator was not found");
    if (annotator.status !== "active") {
      throw conflict("annotator_inactive", "only an active annotator can be invited");
    }

    const credential = await this.#repository.findCredentialByAnnotatorId(annotatorId);
    let reissued = false;
    if (!credential) {
      const created = await this.#repository.createShadowCredential({
        annotatorId,
        email,
        decoyPasswordHash: await createDecoyPasswordHash(),
      });
      if (!created) {
        throw conflict("email_in_use", "an annotator with this address already exists");
      }
    } else {
      if (credential.status === "active") {
        throw conflict(
          "credential_active",
          "this account is already activated; trigger a password reset instead",
        );
      }
      if (credential.status === "suspended") {
        throw conflict("credential_suspended", "a suspended credential cannot be invited");
      }
      // The stored address is the account identity; a different address on a
      // re-issue would silently re-point the account at another mailbox, which
      // is an edit this endpoint must not smuggle in.
      if (credential.email !== email) {
        throw conflict(
          "email_mismatch",
          "the address does not match the stored credential; correct it first",
        );
      }
      reissued = true;
    }

    const issued = await this.#issueClaim(
      annotatorId,
      this.#config.challenge.inviteTtlHours * 3_600,
    );
    await this.#audit(actor, context, "annotator.credential_invited", annotatorId, {
      reissued,
      delivery: "mail",
    });
    await this.#deliverBestEffort(
      email,
      annotatorClaimMail(issued.claimUrl, this.#config.challenge.inviteTtlHours),
      { annotatorId, reason: "account_claim" },
    );
    return issued;
  }

  /**
   * Operator-triggered password reset (docs/34 §6.5): a claim link against an
   * *active* credential, whose confirmation swaps the password and revokes
   * every session. The operator sees the link once and never any secret the
   * annotator ends up with.
   */
  async triggerPasswordReset(
    actor: AuthenticatedSession,
    annotatorId: string,
    context: RequestContext,
  ): Promise<ClaimIssued> {
    this.#guard(actor);

    const credential = await this.#repository.findCredentialByAnnotatorId(annotatorId);
    if (!credential) throw notFound("annotator_not_found", "the annotator has no credential");
    if (credential.status !== "active" || credential.annotator_status !== "active") {
      throw conflict(
        "credential_not_active",
        "only an activated account can be reset; use the claim invite for pending ones",
      );
    }

    const ttlSeconds = this.#config.challenge.passwordResetTtlMinutes * 60;
    const issued = await this.#issueClaim(annotatorId, ttlSeconds);
    await this.#audit(actor, context, "annotator.password_reset_triggered", annotatorId, {
      delivery: "mail",
    });
    await this.#deliverBestEffort(
      credential.email,
      annotatorResetLinkMail(issued.claimUrl, this.#config.challenge.passwordResetTtlMinutes),
      { annotatorId, reason: "password_reset_link" },
    );
    return issued;
  }

  /**
   * Operator-side profile edit. `identity.write` without sudo — the same level
   * as editing an alias, because contact facts are operational data, not a
   * credential or an authorization (docs/34 A11).
   */
  async updateProfile(
    actor: AuthenticatedSession,
    annotatorId: string,
    profile: ProfileInput,
    context: RequestContext,
  ): Promise<ProfileRow | null> {
    requireCapability(actor.operator.role, "identity.write");

    const annotator = await this.#repository.findAnnotator(annotatorId);
    if (!annotator) throw notFound("annotator_not_found", "the annotator was not found");
    if (annotator.status === "tombstoned") {
      // The wipe trigger just emptied this row; writing fresh contact data back
      // onto a tombstone would undo a purge.
      throw conflict("annotator_tombstoned", "a tombstoned annotator has no profile to edit");
    }

    await this.#repository.upsertProfile(annotatorId, profile);
    await this.#audit(actor, context, "annotator.profile_updated", annotatorId, {
      fields: Object.keys(profile).sort(),
    });
    return this.#repository.getProfile(annotatorId);
  }

  async #issueClaim(annotatorId: string, ttlSeconds: number): Promise<ClaimIssued> {
    const token = generateOpaqueToken();
    const salt = generateSalt();
    const challengeId = await this.#repository.issueChallenge({
      annotatorId,
      purpose: "account_claim",
      codeSalt: salt,
      codeHash: hashChallengeCode(salt, token),
      ttlSeconds,
    });
    const url = new URL("/claim-account", this.#config.publicOrigin);
    url.searchParams.set("challenge", challengeId);
    url.searchParams.set("token", token);
    return { annotatorId, claimUrl: url.toString(), expiresInSeconds: ttlSeconds };
  }

  #guard(actor: AuthenticatedSession): void {
    requireCapability(actor.operator.role, "identity.write");
    if (!actor.session.sudoActive) {
      throw forbidden("sudo_required", "re-enter your password to confirm this action");
    }
  }

  async #audit(
    actor: AuthenticatedSession,
    context: RequestContext,
    action: string,
    annotatorId: string,
    details: Record<string, unknown>,
  ): Promise<void> {
    await this.#auditSink.recordAudit({
      actorType: "operator",
      actorId: actor.operator.id,
      action,
      entityType: "annotator",
      entityId: annotatorId,
      requestId: context.requestId,
      details: { ...details, operator: actor.operator.email },
    });
  }

  /**
   * Mail is advisory here: the claim URL is already in the operator's hands,
   * so a delivery failure downgrades to `delivery: manual` in a follow-up
   * audit row rather than failing a request that already succeeded.
   */
  async #deliverBestEffort(
    email: string,
    mail: Omit<ConsoleMail, "to">,
    audit: { annotatorId: string; reason: string },
  ): Promise<void> {
    try {
      await this.#mailer.send({ to: email, ...mail });
    } catch (error) {
      const errorName = error instanceof Error ? error.name : "unknown";
      try {
        await this.#auditSink.recordAudit({
          actorType: "service",
          actorId: null,
          action: "annotator.mail_delivery_failed",
          entityType: "annotator",
          entityId: audit.annotatorId,
          // The class name only — a provider error can carry the address, and
          // this row is not the place for it.
          details: { reason: audit.reason, error: errorName, delivery: "manual" },
        });
      } catch {
        // The claim already exists and the operator holds the URL. Nothing
        // left worth failing for.
      }
    }
  }
}
