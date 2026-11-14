import { randomUUID } from "node:crypto";

import {
  currentCatalogReleaseSql,
  type CatalogReleaseForDelivery,
} from "@dataminer/api/catalog-delivery";

import type { SqlClient } from "./sql.js";

/**
 * The annotator-facing repository, backed by its own pool and its own database
 * role (docs/11 §4.2). It never touches `operators`, `operator_sessions`, or
 * `operator_email_challenges` — and once `dataminer_app_staging` is granted in a
 * real Cloud SQL instance, it *cannot*, which is the point: a routing mistake in
 * the public surface must not become a governance write.
 *
 * Statements that carry the interesting invariants are exported so the tests
 * execute them against a real PostgreSQL engine instead of a copy.
 */

/**
 * Atomic redemption, the same shape Loopback uses (11 §5.1). The row lock makes
 * a concurrent race for the last use safe: whichever transaction gets there
 * first increments, the other matches no row.
 *
 * `used_at` and `used_by_annotator_id` are written through COALESCE, so they
 * record the *first* redemption and stay stable afterwards — they are a
 * denormalized convenience for the console list, not the relation itself.
 *
 * No rows back means unknown, revoked, exhausted, or expired. The caller must
 * not tell those apart to the client.
 */
export const redeemInviteSql = `UPDATE invite_codes
      SET used_count = used_count + 1,
          used_at = COALESCE(used_at, transaction_timestamp()),
          used_by_annotator_id = COALESCE(used_by_annotator_id, $2)
    WHERE code = $1
      AND status = 'active'
      AND used_count < max_uses
      AND (expires_at IS NULL OR expires_at > transaction_timestamp())
    RETURNING code`;

/**
 * Lookup for login and for signup's address collision check.
 *
 * Deliberately returns the row for suspended accounts too. Login has to spend
 * the same argon2 verification on a suspended account as on a live one, or the
 * response time answers a question the error message refuses to.
 */
export const findCredentialByEmailSql = `SELECT c.annotator_id, c.email, c.password_hash, c.status,
            c.failed_login_count, c.locked_until, a.status AS annotator_status
       FROM annotator_credentials c
       JOIN annotators a ON a.id = c.annotator_id
      WHERE c.email = $1`;

/**
 * Activation. Guarded on `pending_verification` so a replayed verification
 * cannot reset an account that has since been suspended, and the CHECK on the
 * table refuses `active` without `email_verified_at` regardless.
 */
export const activateCredentialSql = `UPDATE annotator_credentials
      SET status = 'active',
          email_verified_at = COALESCE(email_verified_at, transaction_timestamp()),
          failed_login_count = 0,
          locked_until = NULL
    WHERE annotator_id = $1
      AND status = 'pending_verification'
    RETURNING annotator_id`;

/**
 * Password reset completion.
 *
 * Guarded on `active` on purpose. A `pending_verification` account must finish
 * signup verification instead — letting a reset activate it would turn "I can
 * receive mail at this address" into a second, quieter activation path that
 * skips the invite bookkeeping. A suspended account stays suspended: knowing
 * the mailbox is not the thing that was revoked.
 *
 * Clearing `failed_login_count` / `locked_until` is the point of the flow for
 * a user who is locked out — a reset that left them locked would be no
 * recovery at all. `password_updated_at` moves so the audit trail can tell a
 * reset apart from an unchanged credential.
 */
export const resetCredentialPasswordSql = `UPDATE annotator_credentials
      SET password_hash = $2,
          password_updated_at = transaction_timestamp(),
          failed_login_count = 0,
          locked_until = NULL
    WHERE annotator_id = $1
      AND status = 'active'
    RETURNING annotator_id`;

/**
 * Every live session for one annotator, revoked in one statement.
 *
 * A password reset must not leave a stolen session token working: the whole
 * reason someone resets is that the old secret may be in someone else's hands.
 * The device credentials issued to enrolled machines are deliberately NOT
 * touched — they are a separate credential kind bound to a machine, and
 * revoking them would silently stop uploads from hardware that was never
 * compromised (11 §7.4). Rotating those stays an explicit, per-device action.
 */
export const revokeAllSessionsForAnnotatorSql = `UPDATE annotator_sessions
      SET revoked_at = transaction_timestamp(), revoke_reason = $2
    WHERE annotator_id = $1 AND revoked_at IS NULL`;

/**
 * Session resolution. Expiry, revocation, credential status, and annotator
 * status are all evaluated in the one statement, so suspending a user takes
 * effect on their next request rather than whenever their token happens to
 * expire — which is the entire reason for a server-side session table
 * (10 §12.7).
 */
export const findSessionSql = `SELECT s.id, s.annotator_id, s.expires_at
       FROM annotator_sessions s
       JOIN annotator_credentials c ON c.annotator_id = s.annotator_id
       JOIN annotators a ON a.id = s.annotator_id
      WHERE s.token_hash = $1
        AND s.revoked_at IS NULL
        AND s.expires_at > transaction_timestamp()
        AND c.status = 'active'
        AND a.status = 'active'`;

/**
 * Sliding renewal (docs/34 §6.1a). Annotator sessions stay alive as long as
 * they are used: when a resolved session has burnt through more than half its
 * window, expiry is pushed back out to a full window. The half-window
 * predicate is the write-amplification control — a busy client renews at most
 * once per half window instead of on every request.
 *
 * The guards repeat everything `findSessionSql` checks. Between the read and
 * this write a session can be revoked or its owner suspended, and a renewal
 * that resurrected either would quietly extend a credential someone just
 * killed. A renewal must never make a dead session less dead.
 */
export const renewSessionSql = `UPDATE annotator_sessions s
      SET expires_at = transaction_timestamp() + make_interval(secs => $2),
          last_seen_at = transaction_timestamp()
     FROM annotator_credentials c, annotators a
    WHERE s.token_hash = $1
      AND c.annotator_id = s.annotator_id
      AND a.id = s.annotator_id
      AND s.revoked_at IS NULL
      AND s.expires_at > transaction_timestamp()
      AND s.expires_at < transaction_timestamp() + make_interval(secs => $3)
      AND c.status = 'active'
      AND a.status = 'active'
    RETURNING s.id`;

/**
 * Every live session except the caller's own. A password *change* (the user is
 * present, typed the old password, and keeps working) kicks every other holder
 * of the account out without interrupting the very session that proved itself;
 * a *reset* still revokes everything including the caller, via
 * `revokeAllSessionsForAnnotatorSql`.
 */
export const revokeOtherSessionsForAnnotatorSql = `UPDATE annotator_sessions
      SET revoked_at = transaction_timestamp(), revoke_reason = $3
    WHERE annotator_id = $1
      AND token_hash <> $2
      AND revoked_at IS NULL`;

/**
 * Device enrollment creates only the stable device identity and credential.
 * Profile compatibility is selected per game and proved by recording preflight;
 * legacy profile columns/tables are retained for audit but are not written here.
 *
 * Status is `active` on insert per D6 — holding a valid invite was already the
 * authorization decision, so there is no second gate here. `pending` stays in
 * the enum so device-side review can be tightened later without touching signup.
 */
export const enrollDeviceSql = `WITH resolved_device AS (
         INSERT INTO devices(
           id, client_device_id, annotator_id, certified_profile_id,
           credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
           display_snapshot, input_devices_snapshot, client_version
         ) VALUES ($1, $2, $3, NULL, $4, 'active', $5, $6, $7, $8, $9, $10)
         ON CONFLICT (client_device_id) DO UPDATE
           SET client_device_id = EXCLUDED.client_device_id
         WHERE devices.annotator_id = EXCLUDED.annotator_id
         RETURNING id, id = $1 AS created, status
       )
       SELECT r.id, r.created FROM resolved_device r`;

/**
 * Rotation, scoped to the caller's own devices. The `annotator_id` predicate is
 * the ownership check — without it a logged-in user could rotate anyone's
 * credential by guessing a device id.
 */
export const rotateDeviceCredentialSql = `UPDATE devices
      SET credential_fingerprint = $3
    WHERE id = $1
      AND annotator_id = $2
      AND status IN ('pending', 'active')
    RETURNING id`;

/**
 * Attempt budget and single-use consumption, mirroring the operator challenge
 * discipline (10 §4.3). A six digit code is not protected by hash strength: the
 * protection is a short TTL, one use, and a hard attempt cap — so the attempt is
 * claimed *before* the code is compared, and a claim that would exceed the cap
 * returns nothing.
 */
export const claimChallengeAttemptSql = `UPDATE annotator_email_challenges
      SET attempt_count = attempt_count + 1
    WHERE id = $1
      AND consumed_at IS NULL
      AND superseded_at IS NULL
      AND expires_at > transaction_timestamp()
      AND attempt_count < $2
    RETURNING id, annotator_id, purpose, code_salt, code_hash, attempt_count`;

export const consumeChallengeSql = `UPDATE annotator_email_challenges
      SET consumed_at = transaction_timestamp()
    WHERE id = $1
      AND consumed_at IS NULL
      AND superseded_at IS NULL
      AND expires_at > transaction_timestamp()
    RETURNING id`;

/**
 * Failure-only diagnostic lookup for a rejected challenge claim. The public
 * response remains deliberately indistinguishable; this classification exists
 * solely so an access-controlled server log can tell an expired code from a
 * superseded one without ever logging the challenge id or code itself.
 */
export const classifyChallengeRejectionSql = `SELECT purpose,
            consumed_at IS NOT NULL AS consumed,
            superseded_at IS NOT NULL AS superseded,
            expires_at <= transaction_timestamp() AS expired,
            attempt_count >= $2 AS attempts_exhausted
       FROM annotator_email_challenges
      WHERE id = $1`;

/**
 * Issuing a new code retires the open ones for that purpose, so a resend cannot
 * leave two valid codes in a mailbox at once.
 */
export const supersedeOpenChallengesSql = `UPDATE annotator_email_challenges
      SET superseded_at = transaction_timestamp()
    WHERE annotator_id = $1
      AND purpose = $2
      AND consumed_at IS NULL
      AND superseded_at IS NULL`;

/**
 * Shared fixed-window counter. The table is the same one the console surface
 * uses; the bucket key spaces are kept disjoint by the `app_` prefix every
 * caller in this file applies, so end-user traffic cannot exhaust an operator's
 * window or vice versa (11 §4.2).
 */
export const consumeRateLimitSql = `INSERT INTO console_rate_limits(bucket, window_started_at, counter)
       VALUES ($1, transaction_timestamp(), 1)
       ON CONFLICT (bucket) DO UPDATE
          SET window_started_at = CASE
                WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                     <= transaction_timestamp()
                THEN transaction_timestamp()
                ELSE console_rate_limits.window_started_at
              END,
              counter = CASE
                WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                     <= transaction_timestamp()
                THEN 1
                ELSE console_rate_limits.counter + 1
              END,
              updated_at = transaction_timestamp()
       RETURNING counter, window_started_at`;

/**
 * Exponential lockout on repeated failures, character for character the
 * operator statement in `repository.ts`. Two accounts systems drifting apart on
 * something this fiddly is how the units and the ladder ended up different in
 * the first place; keeping the text identical is the point.
 *
 * `$3`/`$4` are **minutes**, matching `mins =>`. The caller must not pre-convert.
 *
 * The exponent is clamped before it reaches `power`. `power(int, int)` resolves
 * to the double-precision overload — the `::numeric` casts do not change that,
 * the product is promoted to double — so an unclamped exponent overflows float8
 * at around ten thousand failures and every later failed login on that account
 * raises instead of locking it. 30 doublings is already far past the cap.
 */
const lockoutIntervalSql = `make_interval(mins => LEAST(
                   $4::numeric,
                   $3::numeric * power(2, LEAST(GREATEST((failed_login_count + 1) / $2 - 1, 0), 30))
                 )::int)`;

export const registerLoginFailureSql = `UPDATE annotator_credentials
      SET failed_login_count = failed_login_count + 1,
          locked_until = CASE
            WHEN (failed_login_count + 1) % $2 = 0
            THEN transaction_timestamp() + ${lockoutIntervalSql}
            ELSE locked_until
          END
    WHERE annotator_id = $1
    RETURNING failed_login_count, locked_until`;

/**
 * Read-only check that a code could be redeemed right now.
 *
 * This exists so signup can reject an unusable code *before* it looks the
 * address up. Doing the address check first turned the endpoint into an
 * oracle: a registered address answered 202 and an unregistered one 403, so
 * anyone could enumerate accounts with a code they made up. Probing now costs
 * a real code, which is the exposure 11 §12.2 already accepted.
 *
 * It is deliberately not a reservation. The redemption in the signup
 * transaction stays the only thing that spends a use, and it re-checks every
 * predicate under a row lock — this is a gate, not a claim.
 */
export const inviteRedeemableSql = `SELECT code
       FROM invite_codes
      WHERE code = $1
        AND status = 'active'
        AND used_count < max_uses
        AND (expires_at IS NULL OR expires_at > transaction_timestamp())`;

/**
 * Resolves a signup challenge back to the address it was issued for, so a
 * verification code can be reissued when the first mail never arrived.
 *
 * Expiry and supersession are deliberately *not* filtered: the challenge id is
 * the client's proof that it is the party that signed up, and the whole point
 * of a resend is that the previous code is gone or stale. What is filtered is
 * the account state — only an unverified credential on a live annotator can be
 * resent to, so this can never mail an active account or a suspended one.
 */
export const findSignupChallengeForResendSql = `SELECT ch.id, ch.annotator_id, c.email
       FROM annotator_email_challenges ch
       JOIN annotator_credentials c ON c.annotator_id = ch.annotator_id
       JOIN annotators a ON a.id = ch.annotator_id
      WHERE ch.id = $1
        AND ch.purpose = 'signup_verify'
        AND ch.consumed_at IS NULL
        AND c.status = 'pending_verification'
        AND a.status = 'active'`;

/**
 * Lookup by annotator id, for flows that already authenticated a session and
 * need the stored email or hash (password change, claim confirmation). Same
 * column shape as the by-email lookup so the two stay interchangeable.
 */
export const findCredentialByAnnotatorIdSql = `SELECT c.annotator_id, c.email, c.password_hash, c.status,
            c.failed_login_count, c.locked_until, a.status AS annotator_status
       FROM annotator_credentials c
       JOIN annotators a ON a.id = c.annotator_id
      WHERE c.annotator_id = $1`;

/**
 * Claim-link resolution for the public preflight (docs/34 §6.5): the landing
 * page needs to know which branch it is rendering — activation (terms +
 * password) or reset (password only) — before the user submits anything. Reads
 * only; the attempt budget is charged by the confirm step, so previewing a
 * link does not burn it.
 */
export const findClaimChallengeSql = `SELECT ch.id, ch.annotator_id, c.email, c.status AS credential_status
       FROM annotator_email_challenges ch
       JOIN annotator_credentials c ON c.annotator_id = ch.annotator_id
       JOIN annotators a ON a.id = ch.annotator_id
      WHERE ch.id = $1
        AND ch.purpose = 'account_claim'
        AND ch.consumed_at IS NULL
        AND ch.superseded_at IS NULL
        AND ch.expires_at > transaction_timestamp()
        AND a.status = 'active'`;

/**
 * Claim activation (docs/34 §5.2): the shadow credential becomes a real one.
 * Guarded on `pending_verification` exactly like `activateCredentialSql` —
 * an active account claims through the reset branch instead, and a suspended
 * one does not claim at all. Redeeming the link is what proves the mailbox
 * (the link travelled through it, or was handed to its owner), so
 * `email_verified_at` is stamped here.
 */
export const claimActivateCredentialSql = `UPDATE annotator_credentials
      SET password_hash = $2,
          status = 'active',
          email_verified_at = COALESCE(email_verified_at, transaction_timestamp()),
          password_updated_at = transaction_timestamp(),
          failed_login_count = 0,
          locked_until = NULL
    WHERE annotator_id = $1
      AND status = 'pending_verification'
    RETURNING annotator_id`;

/**
 * Whole-object replace, matching the PUT contract: a field the caller omitted
 * is cleared, not preserved — that is how a stale phone number gets removed.
 */
export const upsertProfileSql = `INSERT INTO annotator_profiles(
         annotator_id, display_name, phone, im_kind, im_handle, region
       ) VALUES ($1, $2, $3, $4, $5, $6)
       ON CONFLICT (annotator_id) DO UPDATE
          SET display_name = EXCLUDED.display_name,
              phone = EXCLUDED.phone,
              im_kind = EXCLUDED.im_kind,
              im_handle = EXCLUDED.im_handle,
              region = EXCLUDED.region`;

/**
 * Thrown inside the signup transaction when the code cannot be redeemed, so the
 * annotator row inserted a moment earlier is rolled back rather than committed
 * as an orphan. Caught at the boundary of `signup`, which turns it into a null.
 */
class InviteUnavailable extends Error {}

/** One parameter order for every profile write, so the SQL and its callers
 * cannot drift field-by-field. `undefined` becomes NULL — replace semantics. */
function profileParams(annotatorId: string, profile: ProfileFields): unknown[] {
  return [
    annotatorId,
    profile.displayName ?? null,
    profile.phone ?? null,
    profile.imKind ?? null,
    profile.imHandle ?? null,
    profile.region ?? null,
  ];
}

export type ChallengePurpose = "signup_verify" | "password_reset" | "account_claim";

export type ChallengeRejectionReason =
  | "challenge_missing"
  | "challenge_wrong_purpose"
  | "challenge_consumed"
  | "challenge_superseded"
  | "challenge_expired"
  | "challenge_attempts_exhausted"
  | "challenge_unavailable";

export type ProfileFields = {
  displayName?: string | undefined;
  phone?: string | undefined;
  imKind?: string | undefined;
  imHandle?: string | undefined;
  region?: string | undefined;
};

export type ProfileRow = {
  displayName: string | null;
  phone: string | null;
  imKind: string | null;
  imHandle: string | null;
  region: string | null;
  updatedAt: string;
};

export type CredentialRow = {
  annotator_id: string;
  email: string;
  password_hash: string;
  status: "pending_verification" | "active" | "suspended";
  failed_login_count: number;
  locked_until: Date | null;
  annotator_status: "active" | "suspended" | "tombstoned";
};

export type EnrolledDevice = {
  id: string;
  clientDeviceId: string;
  created: boolean;
};

export type SignupInput = {
  code: string;
  email: string;
  passwordHash: string;
  alias: string;
  profile?: ProfileFields | null;
  consent: {
    policyVersion: string;
    purpose: string;
    termsHash: string;
    /** Server-configured per terms version (docs/49 E2); never client-supplied. */
    allowsTrainableUse: boolean;
    /**
     * What this terms version authorizes (docs/108 §3.2). Same provenance rule
     * as `allowsTrainableUse` — deployment config, stamped once, never
     * backfilled from whatever the deployment serves later.
     */
    scopes: string[];
  };
  ipHash: string | null;
  userAgent: string | null;
};

export type SignupResult = {
  annotatorId: string;
  consentId: string;
};

export class PostgresAppRepository {
  readonly #pool: SqlClient;
  readonly #transaction: <T>(run: (client: SqlClient) => Promise<T>) => Promise<T>;

  constructor(
    pool: SqlClient,
    transaction: <T>(run: (client: SqlClient) => Promise<T>) => Promise<T>,
  ) {
    this.#pool = pool;
    this.#transaction = transaction;
  }

  /**
   * Signup is one transaction: redeem, create the annotator, store the
   * credential, record the consent, write the redemption ledger row, audit.
   * If redemption fails the whole thing rolls back, so a code can never be spent
   * on an account that was not created.
   *
   * The verification mail is deliberately *not* part of this — it is sent after
   * the commit, because a mail provider outage must not roll back an account the
   * user can simply request a new code for.
   */
  async signup(input: SignupInput): Promise<SignupResult | null> {
    try {
      return await this.#signupTransaction(input);
    } catch (error) {
      if (error instanceof InviteUnavailable) return null;
      throw error;
    }
  }

  async #signupTransaction(input: SignupInput): Promise<SignupResult> {
    return this.#transaction(async (client) => {
      // The annotator row comes first: redemption stamps used_by_annotator_id
      // onto invite_codes, which has a foreign key onto annotators, so the row
      // has to exist before the code can point at it. Both statements are in one
      // transaction, so an invalid code still rolls the annotator back out.
      const annotatorId = randomUUID();
      await client.query(`INSERT INTO annotators(id, alias) VALUES ($1, $2)`, [
        annotatorId,
        input.alias,
      ]);

      const claimed = await client.query<{ code: string }>(redeemInviteSql, [
        input.code,
        annotatorId,
      ]);
      if (claimed.rows.length === 0) throw new InviteUnavailable();

      await client.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, $2, $3)`,
        [annotatorId, input.email, input.passwordHash],
      );

      if (input.profile) {
        await client.query(upsertProfileSql, profileParams(annotatorId, input.profile));
      }

      // Consent is recorded here rather than later because capture_grants has a
      // composite foreign key onto it: without a consent row the operator cannot
      // issue a grant, and without a grant nothing can be captured (11 §1.6).
      // policy_version and terms_hash come from the server's current terms, never
      // from the request body — otherwise a client could claim to have accepted
      // a version it never saw. allows_trainable_use likewise comes from the
      // deployment's terms configuration (docs/49 E2): the operator decided once
      // whether this terms version authorizes trainable use, and every consent
      // stamped with it inherits that decision. It was hardcoded false while no
      // terms version granted the use; the teaser terms (docs/23 §3-4) do, so
      // the value is now configuration, still defaulting to false.
      const consentId = randomUUID();
      await client.query(
        `INSERT INTO consents(
           id, annotator_id, policy_version, purpose, terms_hash, status, accepted_at,
           allows_trainable_use, scopes
         ) VALUES ($1, $2, $3, $4, $5, 'active', transaction_timestamp(), $6, $7::text[])`,
        [
          consentId,
          annotatorId,
          input.consent.policyVersion,
          input.consent.purpose,
          input.consent.termsHash,
          input.consent.allowsTrainableUse,
          input.consent.scopes,
        ],
      );

      await client.query(
        `INSERT INTO annotator_invite_uses(id, code, annotator_id, ip_hash, user_agent)
         VALUES ($1, $2, $3, $4, $5)`,
        [randomUUID(), input.code, annotatorId, input.ipHash, input.userAgent],
      );
      await client.query(
        `INSERT INTO audit_events(event_id, actor_type, actor_id, action, entity_type, entity_id, consent_id, details)
         VALUES ($1, 'annotator', $2, 'annotator.signed_up', 'annotator', $2, $3, $4::jsonb)`,
        [randomUUID(), annotatorId, consentId, JSON.stringify({ inviteCode: input.code })],
      );

      return { annotatorId, consentId };
    });
  }

  async findCredentialByEmail(email: string): Promise<CredentialRow | null> {
    const result = await this.#pool.query<CredentialRow>(findCredentialByEmailSql, [email]);
    return result.rows[0] ?? null;
  }

  async findCredentialByAnnotatorId(annotatorId: string): Promise<CredentialRow | null> {
    const result = await this.#pool.query<CredentialRow>(findCredentialByAnnotatorIdSql, [
      annotatorId,
    ]);
    return result.rows[0] ?? null;
  }

  async findAnnotator(
    annotatorId: string,
  ): Promise<{ id: string; alias: string | null; status: string } | null> {
    const result = await this.#pool.query<{ id: string; alias: string | null; status: string }>(
      `SELECT id, alias, status FROM annotators WHERE id = $1`,
      [annotatorId],
    );
    return result.rows[0] ?? null;
  }

  /**
   * Operator-initiated registration (docs/34 §5.2): annotator row, optional
   * profile, and a shadow credential in one transaction. The decoy password
   * hash satisfies the NOT NULL column without creating anything sign-in-able
   * — status stays `pending_verification` until the claim link is redeemed.
   * Returns null when the address is already taken (unique index), which the
   * service reports as a plain conflict: this surface is authenticated, so
   * there is no enumeration to blind against.
   */
  async registerByOperator(input: {
    email: string;
    alias: string;
    decoyPasswordHash: string;
    profile: ProfileFields | null;
  }): Promise<{ annotatorId: string } | null> {
    try {
      return await this.#transaction(async (client) => {
        const annotatorId = randomUUID();
        await client.query(`INSERT INTO annotators(id, alias) VALUES ($1, $2)`, [
          annotatorId,
          input.alias,
        ]);
        await client.query(
          `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
           VALUES ($1, $2, $3)`,
          [annotatorId, input.email, input.decoyPasswordHash],
        );
        if (input.profile) {
          await client.query(upsertProfileSql, profileParams(annotatorId, input.profile));
        }
        return { annotatorId };
      });
    } catch (error) {
      if ((error as { code?: unknown }).code === "23505") return null;
      throw error;
    }
  }

  /**
   * A shadow credential for an annotator that was hand-created without one
   * (the governance `POST /annotators` path). Same decoy-hash shape as
   * `registerByOperator`; null when the address is taken elsewhere.
   */
  async createShadowCredential(input: {
    annotatorId: string;
    email: string;
    decoyPasswordHash: string;
  }): Promise<boolean> {
    try {
      await this.#pool.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, $2, $3)`,
        [input.annotatorId, input.email, input.decoyPasswordHash],
      );
      return true;
    } catch (error) {
      if ((error as { code?: unknown }).code === "23505") return false;
      throw error;
    }
  }

  async findClaimChallenge(challengeId: string): Promise<{
    id: string;
    annotatorId: string;
    email: string;
    credentialStatus: "pending_verification" | "active" | "suspended";
  } | null> {
    const result = await this.#pool.query<{
      id: string;
      annotator_id: string;
      email: string;
      credential_status: "pending_verification" | "active" | "suspended";
    }>(findClaimChallengeSql, [challengeId]);
    const row = result.rows[0];
    if (!row) return null;
    return {
      id: row.id,
      annotatorId: row.annotator_id,
      email: row.email,
      credentialStatus: row.credential_status,
    };
  }

  /**
   * The activation branch of a claim (docs/34 §5.2): real password in, account
   * active, consent recorded — one transaction, because a half-activated
   * account (password set, no consent) would be able to sign in without ever
   * having accepted anything.
   *
   * The consent insert is guarded by NOT EXISTS: an account that signed up
   * through the form already recorded consent inside the signup transaction,
   * and claiming must not stack a second active row on top of it.
   */
  async claimActivate(input: {
    annotatorId: string;
    passwordHash: string;
    consent: SignupInput["consent"];
  }): Promise<boolean> {
    return this.#transaction(async (client) => {
      const activated = await client.query<{ annotator_id: string }>(claimActivateCredentialSql, [
        input.annotatorId,
        input.passwordHash,
      ]);
      if (activated.rows.length === 0) return false;

      await client.query(
        `INSERT INTO consents(
           id, annotator_id, policy_version, purpose, terms_hash, status, accepted_at,
           allows_trainable_use, scopes
         )
         SELECT $1::uuid, $2::uuid, $3::text, $4::text, $5::text, 'active', transaction_timestamp(), $6::boolean, $7::text[]
          WHERE NOT EXISTS (
            SELECT 1 FROM consents WHERE annotator_id = $2 AND status = 'active'
          )`,
        [
          randomUUID(),
          input.annotatorId,
          input.consent.policyVersion,
          input.consent.purpose,
          input.consent.termsHash,
          input.consent.allowsTrainableUse,
          input.consent.scopes,
        ],
      );
      return true;
    });
  }

  /**
   * The consent this annotator is currently operating under, if any.
   *
   * `ORDER BY accepted_at DESC` rather than "the only row": the schema puts no
   * uniqueness on `annotator_id`, and re-consent deliberately *adds* a row
   * instead of rewriting one — every session pins the consent that was live
   * when it was captured, so rewriting history would relabel data already
   * collected.
   */
  async currentConsent(annotatorId: string): Promise<{
    id: string;
    policyVersion: string;
    termsHash: string;
    purpose: string;
    allowsTrainableUse: boolean;
    scopes: string[];
    acceptedAt: string | null;
  } | null> {
    const result = await this.#pool.query<{
      id: string;
      policy_version: string;
      terms_hash: string;
      purpose: string;
      allows_trainable_use: boolean;
      scopes: string[] | null;
      accepted_at: Date | null;
    }>(
      `SELECT id, policy_version, terms_hash, purpose, allows_trainable_use, scopes, accepted_at
         FROM consents
        WHERE annotator_id = $1 AND status = 'active'
        ORDER BY accepted_at DESC NULLS LAST
        LIMIT 1`,
      [annotatorId],
    );
    const row = result.rows[0];
    if (!row) return null;
    return {
      id: row.id,
      policyVersion: row.policy_version,
      termsHash: row.terms_hash,
      purpose: row.purpose,
      allowsTrainableUse: row.allows_trainable_use,
      scopes: row.scopes ?? [],
      acceptedAt: row.accepted_at ? row.accepted_at.toISOString() : null,
    };
  }

  /**
   * Records a fresh consent and supersedes the previous one, in one transaction.
   *
   * Additive on purpose. The alternative — flipping columns on the existing row
   * — would rewrite what the person is recorded as having agreed to, including
   * for sessions already captured under it. Superseding leaves that history
   * intact and makes the new terms apply from now on.
   */
  async recordConsentRenewal(
    annotatorId: string,
    consent: {
      policyVersion: string;
      purpose: string;
      termsHash: string;
      allowsTrainableUse: boolean;
      scopes: string[];
    },
  ): Promise<string> {
    return this.#transaction(async (client) => {
      const id = randomUUID();
      // Superseded, not revoked (docs/108 D5). This used to write
      // `status='revoked', revoke_reason='superseded_by_renewal'`, and the cost
      // was not cosmetic: `clip_trainability` blocks on `status <> 'active'`, so
      // the moment somebody re-signed to *widen* their authorization, every
      // session they had already captured was judged as consent-withdrawn. The
      // good-faith action was the punished one.
      //
      // The two states also mean different things to purge. Withdrawal is the
      // annotator exercising the terms' withdrawal clause and must delete their
      // data; superseding is the same person still consenting under a newer
      // document. Storing both as `revoked` left nothing able to tell them
      // apart.
      //
      // Insert before retiring, and exclude the new row by id when retiring:
      // `superseded_by` is a foreign key onto this same table, so the successor
      // has to exist before anything can point at it. Both statements are in one
      // transaction, so no other session ever observes the moment when two rows
      // are active — which is the property that matters, because `currentConsent`
      // picks by `accepted_at` and would otherwise depend on clock resolution.
      await client.query(
        `INSERT INTO consents(
           id, annotator_id, policy_version, purpose, terms_hash, status, accepted_at,
           allows_trainable_use, scopes
         ) VALUES ($1, $2, $3, $4, $5, 'active', transaction_timestamp(), $6, $7::text[])`,
        [
          id,
          annotatorId,
          consent.policyVersion,
          consent.purpose,
          consent.termsHash,
          consent.allowsTrainableUse,
          consent.scopes,
        ],
      );
      await client.query(
        `UPDATE consents
            SET status = 'expired',
                superseded_at = transaction_timestamp(),
                superseded_by = $2
          WHERE annotator_id = $1 AND status = 'active' AND id <> $2`,
        [annotatorId, id],
      );
      return id;
    });
  }

  async getProfile(annotatorId: string): Promise<ProfileRow | null> {
    const result = await this.#pool.query<{
      display_name: string | null;
      phone: string | null;
      im_kind: string | null;
      im_handle: string | null;
      region: string | null;
      updated_at: Date;
    }>(
      `SELECT display_name, phone, im_kind, im_handle, region, updated_at
         FROM annotator_profiles
        WHERE annotator_id = $1`,
      [annotatorId],
    );
    const row = result.rows[0];
    if (!row) return null;
    return {
      displayName: row.display_name,
      phone: row.phone,
      imKind: row.im_kind,
      imHandle: row.im_handle,
      region: row.region,
      updatedAt: row.updated_at.toISOString(),
    };
  }

  async upsertProfile(annotatorId: string, profile: ProfileFields): Promise<void> {
    await this.#pool.query(upsertProfileSql, profileParams(annotatorId, profile));
  }

  async revokeOtherSessionsForAnnotator(
    annotatorId: string,
    keepTokenHash: string,
    reason: string,
  ): Promise<void> {
    await this.#pool.query(revokeOtherSessionsForAnnotatorSql, [
      annotatorId,
      keepTokenHash,
      reason,
    ]);
  }

  /** Gate only — the redemption inside the signup transaction still decides. */
  async inviteRedeemable(code: string): Promise<boolean> {
    const result = await this.#pool.query<{ code: string }>(inviteRedeemableSql, [code]);
    return result.rows.length > 0;
  }

  async rotateDeviceCredential(
    deviceId: string,
    annotatorId: string,
    credentialFingerprint: string,
  ): Promise<boolean> {
    const result = await this.#pool.query<{ id: string }>(rotateDeviceCredentialSql, [
      deviceId,
      annotatorId,
      credentialFingerprint,
    ]);
    return result.rows.length > 0;
  }

  async findSignupChallengeForResend(
    challengeId: string,
  ): Promise<{ annotatorId: string; email: string } | null> {
    const result = await this.#pool.query<{ id: string; annotator_id: string; email: string }>(
      findSignupChallengeForResendSql,
      [challengeId],
    );
    const row = result.rows[0];
    if (!row) return null;
    return { annotatorId: row.annotator_id, email: row.email };
  }

  async activateCredential(annotatorId: string): Promise<boolean> {
    const result = await this.#pool.query<{ annotator_id: string }>(activateCredentialSql, [
      annotatorId,
    ]);
    return result.rows.length > 0;
  }

  async resetCredentialPassword(annotatorId: string, passwordHash: string): Promise<boolean> {
    const result = await this.#pool.query<{ annotator_id: string }>(resetCredentialPasswordSql, [
      annotatorId,
      passwordHash,
    ]);
    return result.rows.length > 0;
  }

  async revokeAllSessionsForAnnotator(annotatorId: string, reason: string): Promise<void> {
    await this.#pool.query(revokeAllSessionsForAnnotatorSql, [annotatorId, reason]);
  }

  /**
   * Resolves a session, sliding its expiry when more than half the window is
   * spent (docs/34 §6.1a). `renewed` tells the HTTP layer to refresh the
   * cookie's Max-Age so browser and server expire together.
   */
  async findSession(
    tokenHash: string,
    sliding?: { windowSeconds: number },
  ): Promise<{ id: string; annotatorId: string; renewed: boolean } | null> {
    const result = await this.#pool.query<{
      id: string;
      annotator_id: string;
      expires_at: Date;
    }>(findSessionSql, [tokenHash]);
    const row = result.rows[0];
    if (!row) return null;

    let renewed = false;
    if (sliding) {
      const remainingSeconds = (new Date(row.expires_at).getTime() - Date.now()) / 1000;
      if (remainingSeconds < sliding.windowSeconds / 2) {
        const renewal = await this.#pool.query<{ id: string }>(renewSessionSql, [
          tokenHash,
          sliding.windowSeconds,
          Math.floor(sliding.windowSeconds / 2),
        ]);
        renewed = renewal.rows.length > 0;
      }
    }
    return { id: row.id, annotatorId: row.annotator_id, renewed };
  }

  async issueChallenge(input: {
    annotatorId: string;
    purpose: ChallengePurpose;
    codeSalt: Buffer;
    codeHash: string;
    ttlSeconds: number;
  }): Promise<string> {
    await this.#pool.query(supersedeOpenChallengesSql, [input.annotatorId, input.purpose]);
    const id = randomUUID();
    await this.#pool.query(
      `INSERT INTO annotator_email_challenges(
         id, annotator_id, purpose, code_salt, code_hash, expires_at
       ) VALUES ($1, $2, $3, $4, $5, transaction_timestamp() + make_interval(secs => $6))`,
      [id, input.annotatorId, input.purpose, input.codeSalt, input.codeHash, input.ttlSeconds],
    );
    return id;
  }

  async claimChallengeAttempt(challengeId: string, maxAttempts: number) {
    const result = await this.#pool.query<{
      id: string;
      annotator_id: string;
      purpose: ChallengePurpose;
      code_salt: Buffer | Uint8Array;
      code_hash: string;
      attempt_count: number;
    }>(claimChallengeAttemptSql, [challengeId, maxAttempts]);
    const row = result.rows[0];
    if (!row) return null;
    return {
      id: row.id,
      annotatorId: row.annotator_id,
      purpose: row.purpose,
      codeSalt: Buffer.from(row.code_salt),
      codeHash: row.code_hash,
      attemptCount: row.attempt_count,
    };
  }

  async classifyChallengeRejection(
    challengeId: string,
    expectedPurpose: ChallengePurpose,
    maxAttempts: number,
  ): Promise<ChallengeRejectionReason> {
    const result = await this.#pool.query<{
      purpose: ChallengePurpose;
      consumed: boolean;
      superseded: boolean;
      expired: boolean;
      attempts_exhausted: boolean;
    }>(classifyChallengeRejectionSql, [challengeId, maxAttempts]);
    const row = result.rows[0];
    if (!row) return "challenge_missing";
    if (row.purpose !== expectedPurpose) return "challenge_wrong_purpose";
    if (row.consumed) return "challenge_consumed";
    if (row.superseded) return "challenge_superseded";
    if (row.expired) return "challenge_expired";
    if (row.attempts_exhausted) return "challenge_attempts_exhausted";
    return "challenge_unavailable";
  }

  async consumeChallenge(challengeId: string): Promise<boolean> {
    const result = await this.#pool.query<{ id: string }>(consumeChallengeSql, [challengeId]);
    return result.rows.length > 0;
  }

  async consumeRateLimit(bucket: string, windowSeconds: number, limit: number) {
    const result = await this.#pool.query<{ counter: number; window_started_at: Date }>(
      consumeRateLimitSql,
      [`app_${bucket}`, windowSeconds],
    );
    const row = result.rows[0];
    if (!row) return { allowed: false, retryAfterSeconds: windowSeconds };
    const elapsedSeconds = Math.max(
      0,
      Math.floor((Date.now() - new Date(row.window_started_at).getTime()) / 1000),
    );
    return {
      allowed: Number(row.counter) <= limit,
      retryAfterSeconds: Math.max(1, windowSeconds - elapsedSeconds),
    };
  }

  async registerLoginFailure(
    annotatorId: string,
    policy: { threshold: number; baseMinutes: number; maxMinutes: number },
  ): Promise<void> {
    // Minutes, unconverted — the statement builds a `mins` interval. The
    // seconds conversion that used to live here paired with a `secs` interval,
    // and the two have to move together or the lock is off by sixty.
    await this.#pool.query(registerLoginFailureSql, [
      annotatorId,
      policy.threshold,
      policy.baseMinutes,
      policy.maxMinutes,
    ]);
  }

  async clearLoginFailures(annotatorId: string): Promise<void> {
    await this.#pool.query(
      `UPDATE annotator_credentials
          SET failed_login_count = 0, locked_until = NULL, last_login_at = transaction_timestamp()
        WHERE annotator_id = $1`,
      [annotatorId],
    );
  }

  async issueSession(input: {
    annotatorId: string;
    tokenHash: string;
    absoluteSeconds: number;
    ipHash: string | null;
    userAgent: string | null;
  }): Promise<void> {
    await this.#pool.query(
      `INSERT INTO annotator_sessions(
         id, annotator_id, token_hash, expires_at, ip_hash, user_agent
       ) VALUES ($1, $2, $3, transaction_timestamp() + make_interval(secs => $4), $5, $6)`,
      [
        randomUUID(),
        input.annotatorId,
        input.tokenHash,
        input.absoluteSeconds,
        input.ipHash,
        input.userAgent,
      ],
    );
  }

  async revokeSession(tokenHash: string, reason: string): Promise<void> {
    await this.#pool.query(
      `UPDATE annotator_sessions
          SET revoked_at = transaction_timestamp(), revoke_reason = $2
        WHERE token_hash = $1 AND revoked_at IS NULL`,
      [tokenHash, reason],
    );
  }

  async enrollDevice(input: {
    annotatorId: string;
    clientDeviceId: string;
    credentialFingerprint: string;
    windowsBuild: string;
    gpuPciId: string;
    gpuDriver: string;
    displaySnapshot: unknown;
    inputDevicesSnapshot: unknown;
    clientVersion: string;
  }): Promise<EnrolledDevice> {
    const result = await this.#pool.query<{
      id: string;
      created: boolean;
    }>(enrollDeviceSql, [
      randomUUID(),
      input.clientDeviceId,
      input.annotatorId,
      input.credentialFingerprint,
      input.windowsBuild,
      input.gpuPciId,
      input.gpuDriver,
      JSON.stringify(input.displaySnapshot),
      JSON.stringify(input.inputDevicesSnapshot),
      input.clientVersion,
    ]);
    const row = result.rows[0];
    if (!row) {
      // Scoped resolution inside the CTE prevents one annotator from probing
      // whether another annotator owns the same machine id.
      return {
        id: "",
        clientDeviceId: input.clientDeviceId,
        created: false,
      };
    }
    return {
      id: row.id,
      clientDeviceId: input.clientDeviceId,
      created: row.created,
    };
  }

  /**
   * 绑定前启动读取的当前 release（docs/37 §Bootstrap）。
   *
   * 与摄取面 `currentCatalogRelease` 是同一条查询、同一张表，只是这里跑在
   * `dataminer_app` 角色下——那个角色对 `catalog_releases` 只有 SELECT，发布与
   * 改写仍然只属于 `dataminer_catalog_publisher`（grant-console-roles.sql）。
   * 一台还没绑定的机器要拿到设备配置，就只能从这里读，且只能读。
   */
  async currentCatalogRelease(channel: "dev"): Promise<CatalogReleaseForDelivery[]> {
    const result = await this.#pool.query<{
      catalog_id: string;
      catalog_version: string;
      envelope_bytes: Buffer;
      envelope_sha256: string;
    }>(currentCatalogReleaseSql, [channel]);
    // 与摄取面同形：每游戏一份 bundle 之后返回整个集合（docs/57 §3.3）。
    // 两条路的下发形状必须逐字一致，见 catalog-delivery 的模块注释。
    return result.rows.map((row) => ({
      catalogId: row.catalog_id,
      catalogVersion: row.catalog_version,
      envelopeBytes: Buffer.from(row.envelope_bytes),
      envelopeSha256: row.envelope_sha256,
    }));
  }

  /**
   * Records one telemetry event, ignoring a repeat of an id already stored.
   *
   * The client retries after a timeout without knowing whether the first attempt
   * landed, so `ON CONFLICT DO NOTHING` on the client-generated id is what makes
   * the retry safe. Telemetry is best effort by design: a duplicate is dropped
   * silently rather than failing the batch.
   *
   * `annotator_id` and `consent_id` are resolved from the device row, never
   * taken from the client — the device credential is the only thing that proves
   * who is reporting, and the consent is what makes the row purgeable (11 §6.4).
   *
   * The same statement produces the export outbox row (docs/23 §5): one SQL
   * statement is one transaction, so the event and its outbox pointer land
   * atomically, and a replayed event id (conflict, zero rows inserted) produces
   * no second outbox row. The payload is a *pointer* (`clientEventId`), not the
   * event body — the exporter reads `client_events` at drain time, so a row
   * purged before export is simply never exported.
   */
  async recordClientEvent(input: {
    eventId: string;
    deviceId: string;
    sessionId: string | null;
    occurredAt: string;
    kind: string;
    severity: string;
    clientVersion: string;
    details: unknown;
  }): Promise<void> {
    await this.#pool.query(
      `WITH inserted AS (
         INSERT INTO client_events(
           event_id, device_id, annotator_id, consent_id, session_id,
           occurred_at, kind, severity, client_version, details
         )
         -- Casts are load-bearing: INSERT..SELECT does not infer parameter
         -- types from the target columns under the extended protocol, so the
         -- untyped placeholders 500 on real pg (PGlite's simple protocol
         -- masked this in tests).
         SELECT $1::uuid, d.id, d.annotator_id,
                (SELECT c.id FROM consents c
                  WHERE c.annotator_id = d.annotator_id AND c.status = 'active'
                  ORDER BY c.accepted_at DESC LIMIT 1),
                $3::uuid, $4::timestamptz, $5::text, $6::text, $7::text, $8::jsonb
           FROM devices d
          WHERE d.id = $2
         ON CONFLICT (event_id) DO NOTHING
         RETURNING id, event_id, kind
       )
       INSERT INTO outbox_jobs(event_id, topic, aggregate_type, aggregate_id, dedupe_key, payload)
       SELECT event_id, 'client_event.recorded', 'client_event', event_id::text,
              'client_event:' || event_id::text,
              jsonb_build_object('clientEventId', id, 'kind', kind)
         FROM inserted
       ON CONFLICT DO NOTHING`,
      [
        input.eventId,
        input.deviceId,
        input.sessionId,
        input.occurredAt,
        input.kind,
        input.severity,
        input.clientVersion,
        JSON.stringify(input.details ?? {}),
      ],
    );
  }

  /** Resolves a device credential to its device for the telemetry endpoint. */
  async authenticateDevice(credentialFingerprint: string): Promise<string | null> {
    const result = await this.#pool.query<{ id: string }>(
      `SELECT d.id
         FROM devices d
         LEFT JOIN annotators a ON a.id = d.annotator_id
        WHERE d.credential_fingerprint = $1
          AND d.status IN ('pending', 'active')
          AND (d.annotator_id IS NULL OR a.status = 'active')`,
      [credentialFingerprint],
    );
    return result.rows[0]?.id ?? null;
  }

  async listDevices(annotatorId: string) {
    const result = await this.#pool.query<{
      id: string;
      client_device_id: string;
      status: string;
      client_version: string;
      last_seen_at: Date | null;
      created_at: Date;
    }>(
      `SELECT id, client_device_id, status, client_version, last_seen_at, created_at
         FROM devices
        WHERE annotator_id = $1
        ORDER BY created_at DESC, id DESC`,
      [annotatorId],
    );
    return result.rows.map((row) => ({
      deviceId: row.id,
      clientDeviceId: row.client_device_id,
      status: row.status,
      clientVersion: row.client_version,
      lastSeenAt: row.last_seen_at?.toISOString() ?? null,
      createdAt: row.created_at.toISOString(),
    }));
  }

  /**
   * 本人的 consent 回执（docs/34 §4.1 信息架构里 /account 的「条款回执」，
   * docs/65 J-44）。只读、只回本人行；撤回与过期状态一并给出——回执要回答的
   * 是「我签过哪一版、什么时候签的、现在还有效吗」。
   */
  async listConsents(annotatorId: string) {
    const result = await this.#pool.query<{
      id: string;
      policy_version: string;
      purpose: string;
      terms_hash: string;
      status: string;
      allows_trainable_use: boolean;
      scopes: string[] | null;
      accepted_at: Date | null;
      expires_at: Date | null;
      revoked_at: Date | null;
      superseded_at: Date | null;
      created_at: Date;
    }>(
      `SELECT id, policy_version, purpose, terms_hash, status, allows_trainable_use, scopes,
              accepted_at, expires_at, revoked_at, superseded_at, created_at
         FROM consents
        WHERE annotator_id = $1
        ORDER BY created_at DESC, id DESC`,
      [annotatorId],
    );
    return result.rows.map((row) => ({
      consentId: row.id,
      policyVersion: row.policy_version,
      purpose: row.purpose,
      termsHash: row.terms_hash,
      status: row.status,
      allowsTrainableUse: row.allows_trainable_use,
      scopes: row.scopes ?? [],
      acceptedAt: row.accepted_at?.toISOString() ?? null,
      expiresAt: row.expires_at?.toISOString() ?? null,
      revokedAt: row.revoked_at?.toISOString() ?? null,
      // 没有它，本人的回执会把被换版的 consent 显示成 status 的原文 "expired"
      // ——读起来像「你的授权过期了」，而实际上是「你补签了，这份被新的接替」。
      supersededAt: row.superseded_at?.toISOString() ?? null,
      createdAt: row.created_at.toISOString(),
    }));
  }

  async recordAudit(input: {
    actorType: "annotator" | "service";
    actorId: string | null;
    action: string;
    entityType: string;
    entityId: string;
    details?: Record<string, unknown>;
  }): Promise<void> {
    await this.#pool.query(
      `INSERT INTO audit_events(
         event_id, actor_type, actor_id, action, entity_type, entity_id, details
       ) VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)`,
      [
        randomUUID(),
        input.actorType,
        input.actorId,
        input.action,
        input.entityType,
        input.entityId,
        JSON.stringify(input.details ?? {}),
      ],
    );
  }
}
