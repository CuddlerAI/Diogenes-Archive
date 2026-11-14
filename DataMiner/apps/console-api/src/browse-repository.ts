import {
  unattached,
  type AnnotatorListQuery,
  type AuditQuery,
  type ClientEventListQuery,
  type ClipListQuery,
  type DailyGroupBy,
  type DeviceListQuery,
  type OutputDimension,
  type SessionListQuery,
  type StatsOutputQuery,
  type TimeBasis,
  defaultStatsTimeZone,
  defaultTimeBasis,
} from "./browse-contracts.js";
import type {
  AnnotatorDetail,
  AnnotatorSummary,
  ArtifactSummary,
  AuditEntry,
  BrowseRepository,
  CaptureRunSummary,
  ClientEventEntry,
  ClipDetail,
  ClipHealth,
  ClipSummary,
  ConsentSummary,
  Contribution,
  ContributionDay,
  DailyIntake,
  DailyIntakeDay,
  DeviceDetail,
  DeviceSummary,
  GameDistribution,
  GameDistributionRow,
  GameSummary,
  GrantSummary,
  OutputMetrics,
  OutputReport,
  OutputRow,
  PipelineHealth,
  PipelineOverview,
  QualityBacklog,
  SessionArtifactSummary,
  SessionDetail,
  SessionSummary,
  SessionTrainability,
  VerificationAttempt,
} from "./browse-domain.js";
import { decodeCursor, toPage, type Page } from "./pagination.js";
import type { SqlClient } from "./sql.js";

// pg returns bigint and numeric as strings to avoid precision loss. Counts and
// byte totals in this schema stay well inside the safe integer range, so they are
// converted once here instead of leaking strings into the API surface.
function count(value: unknown): number {
  if (value === null || value === undefined) return 0;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : 0;
}

function iso(value: Date | null | undefined): string | null {
  return value ? value.toISOString() : null;
}

/** Accumulates positional parameters so filters compose without index bookkeeping. */
class Params {
  readonly values: unknown[] = [];

  add(value: unknown): string {
    this.values.push(value);
    return `$${this.values.length}`;
  }
}

const storedBytesForClip = `(
  SELECT COALESCE(sum(ao.expected_size_bytes), 0)
    FROM artifact_objects ao
   WHERE ao.clip_id = c.id AND ao.storage_state = 'stored'
)`;

/**
 * QA 队列的超期阈值（小时）。24 来自 issue #855 §C 给 QA 定的运行目标。
 *
 * 只有这一个定义点：SQL 里的 FILTER 与回给界面的 `overdueThresholdHours` 都读它，
 * 所以界面上写着"超过 N 小时"的那个 N，就是数出这个计数用的那个 N。
 */
const qualityReviewOverdueHours = 24;

/**
 * depth 是 clip 的**第七件、可选**产物（[docs/105](../../../docs/105-depth-into-ingestion-plane-decision.md)）。
 *
 * # 为什么它需要自己的口径，而不是躺在 `storedBytes` 里
 *
 * 摄取面认 depth 之后，它的字节混进了 `storedBytes` 的总和，件数混进了
 * `clipCount`——于是后台能回答「昨天进了多少数据」，却答不出**「其中 depth 有多少」**。
 * 而这两个数在运营上完全不是一件事：
 *
 * - depth 只有装了实验包、且该游戏做过 profile 标定的机器才产出，所以「有几条带
 *   depth」直接就是那条线的产能读数，混在总数里等于没有；
 * - 实测 depth ≈ video 的 **2.08 倍**（[docs/109 §4.2](../../../docs/109-formal-collection-ignition-readiness-review.md)），
 *   一条带 depth 的 clip 抬总字节 ×3。存储曲线上一个陡坡，看不出是「多采了三倍」
 *   还是「有人开了 depth」——这两种情况的处置完全相反。
 *
 * 按 `kind` 过滤而不是按 `meta_json` 声明：声明漏写就会漏计，而 `artifact_objects`
 * 行是服务端自己写的（迁移 0030 之后 `artifact_kind` 含 `depth`）。
 * 与 [`storedBytesForClip`] 同样只数 `stored`——没落地的字节不该出现在存储读数里。
 */
const depthBytesForClip = `(
  SELECT COALESCE(sum(ao.expected_size_bytes), 0)
    FROM artifact_objects ao
   WHERE ao.clip_id = c.id AND ao.storage_state = 'stored' AND ao.kind = 'depth'
)`;

/** 这个 clip 是否带已落地的 depth 产物。 */
const clipHasDepth = `EXISTS (
  SELECT 1 FROM artifact_objects ao
   WHERE ao.clip_id = c.id AND ao.storage_state = 'stored' AND ao.kind = 'depth'
)`;

/**
 * 一个片段的录制墙钟（docs/67 §3.3）。所属 run 的起始时间是最贴近「这一段是什么
 * 时候录的」的服务端事实；run 没有起始时间（老数据）时退回登记时间，宁可退化成
 * 旧口径，也不要在图上凭空少一天的数据。
 */
const clipRecordedAt = `COALESCE(r.started_at, c.created_at)`;

/** 片段时长（秒）。fps 是有理数（docs/50 P-8），所以按分子分母算而不是取整。 */
const clipDurationSeconds = `CASE WHEN r.fps_num > 0
       THEN c.n_frames::numeric * r.fps_den / r.fps_num
       ELSE 0 END`;

const clipHealthNumber = (field: string) =>
  `COALESCE((c.health_json ->> '${field}')::numeric, 0)`;

/**
 * 补帧占比上限，与 QC 脚本同一阈值
 * （`tools/quality-check/sopv2_video_check.py` 的 `DUP_MAX_FRAC`，v1 档）。
 * 两边判的是同一件事——「补帧多到影响可训练性」；阈值分叉的话，console 报异常
 * 而 QC 放行（或反过来）就成了两本账。
 */
const DUP_MAX_FRAC = 0.01;

/**
 * 片段健康异常（docs/146）：硬故障位任一非零，或补帧占比越线。
 *
 * `sourceSuperseded` **不参与判定**——它是 CFR 选帧的正常计数：游戏渲染快于
 * 30fps 采集档时，每秒必然有 ~30 帧新源帧在呈现前顶替旧帧，docs/111 §3.3 已判
 * 「这不是缺陷」。拿它当异常位，生产语料会 100% 报异常（docs/146 §1 的事故）。
 * 它仍随 `toClipHealth` 原样出现在片段行里，是诊断列不是判定项。
 *
 * `duplicateFrames` 按占比而不是「任一非零」：CFR 下偶发一两帧补帧是健康形态
 * （实测健康条 1/1893）。`n_frames` 有 NOT NULL + 正数 CHECK，比例恒可算。
 * 缺字段按 0 处理：老片段没有这些键。
 */
const clipIsUnhealthy = `(
  ${clipHealthNumber("queueDrops")} > 0
  OR ${clipHealthNumber("inputOverflow")} > 0
  OR ${clipHealthNumber("duplicateFrames")} > ${DUP_MAX_FRAC} * c.n_frames
)`;

export class PostgresBrowseRepository implements BrowseRepository {
  readonly #pool: SqlClient;

  constructor(pool: SqlClient) {
    this.#pool = pool;
  }

  async listAnnotators(query: AnnotatorListQuery): Promise<Page<AnnotatorSummary>> {
    const params = new Params();
    const filters: string[] = [];
    if (query.status) filters.push(`a.status = ${params.add(query.status)}`);
    if (query.query) filters.push(`a.alias ILIKE ${params.add(`%${query.query}%`)}`);
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(a.created_at, a.id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::uuid)`,
      );
    }
    const limit = params.add(query.pageSize + 1);

    const result = await this.#pool.query<AnnotatorRow>(
      `SELECT a.id, a.alias, a.status, a.created_at,
              (SELECT count(*) FROM devices d WHERE d.annotator_id = a.id) AS device_count,
              (SELECT count(*) FROM consents con
                WHERE con.annotator_id = a.id AND con.status = 'active') AS active_consent_count,
              (SELECT count(*) FROM capture_grants g
                WHERE g.annotator_id = a.id AND g.status = 'active') AS active_grant_count,
              roll.session_count, roll.last_activity_at,
              roll.clip_count, roll.verified_clip_count, roll.stored_bytes
         FROM annotators a
         LEFT JOIN LATERAL (${annotatorRollup("a.id")}) roll ON TRUE
        ${where(filters)}
        ORDER BY a.created_at DESC, a.id DESC
        LIMIT ${limit}`,
      params.values,
    );

    return toPage(result.rows.map(toAnnotatorSummary), query.pageSize, (row) => ({
      orderedAt: row.createdAt,
      id: row.id,
    }));
  }

  async getAnnotator(id: string): Promise<AnnotatorDetail | null> {
    const base = await this.#pool.query<AnnotatorRow & AnnotatorDetailRow>(
      `SELECT a.id, a.alias, a.status, a.created_at, a.external_subject_hash, a.tombstoned_at,
              (SELECT count(*) FROM devices d WHERE d.annotator_id = a.id) AS device_count,
              (SELECT count(*) FROM consents con
                WHERE con.annotator_id = a.id AND con.status = 'active') AS active_consent_count,
              (SELECT count(*) FROM capture_grants g
                WHERE g.annotator_id = a.id AND g.status = 'active') AS active_grant_count,
              roll.session_count, roll.last_activity_at,
              roll.clip_count, roll.verified_clip_count, roll.stored_bytes
         FROM annotators a
         LEFT JOIN LATERAL (${annotatorRollup("a.id")}) roll ON TRUE
        WHERE a.id = $1`,
      [id],
    );
    const row = base.rows[0];
    if (!row) return null;

    const [consents, grants, devices, identity, profile] = await Promise.all([
      this.#pool.query<ConsentRow>(
        `SELECT id, policy_version, purpose, status, allows_trainable_use, scopes,
                accepted_at, expires_at, revoked_at, superseded_at, retention_state
           FROM consents WHERE annotator_id = $1 ORDER BY created_at DESC`,
        [id],
      ),
      this.#pool.query<GrantRow>(
        `SELECT id, game_id, device_id, status, allows_trainable,
                valid_from, valid_until, issued_by, revoked_at
           FROM capture_grants WHERE annotator_id = $1 ORDER BY issued_at DESC`,
        [id],
      ),
      this.listDevices({ annotatorId: id, pageSize: 200 }),
      // Left joins throughout: an operator-created annotator has no credential
      // and no redemption, and the row should still come back with nulls rather
      // than vanish. The invite operator's email is looked up by the id stored
      // on the code (invite_codes.created_by).
      this.#pool.query<{
        email: string;
        credential_status: "pending_verification" | "active" | "suspended";
        registered_at: Date;
        email_verified_at: Date | null;
        last_login_at: Date | null;
        invite_code: string | null;
        invited_by: string | null;
        redeemed_at: Date | null;
      }>(
        `SELECT cr.email, cr.status AS credential_status, cr.created_at AS registered_at,
                cr.email_verified_at, cr.last_login_at,
                use.code AS invite_code, op.email AS invited_by, use.used_at AS redeemed_at
           FROM annotator_credentials cr
           LEFT JOIN annotator_invite_uses use ON use.annotator_id = cr.annotator_id
           LEFT JOIN invite_codes ic ON ic.code = use.code
           LEFT JOIN operators op ON op.id = ic.created_by
          WHERE cr.annotator_id = $1
          ORDER BY use.used_at ASC
          LIMIT 1`,
        [id],
      ),
      // Contact facts (docs/34 §5.4). Detail-only on purpose: the list query
      // never touches this table, so no contact data appears on a page that a
      // shoulder-surfer sees by default.
      this.#pool.query<{
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
        [id],
      ),
    ]);

    return {
      ...toAnnotatorSummary(row),
      externalSubjectHash: row.external_subject_hash,
      tombstonedAt: iso(row.tombstoned_at),
      consents: consents.rows.map(
        (consent): ConsentSummary => ({
          id: consent.id,
          policyVersion: consent.policy_version,
          purpose: consent.purpose,
          status: consent.status,
          allowsTrainableUse: consent.allows_trainable_use,
          scopes: consent.scopes ?? [],
          acceptedAt: iso(consent.accepted_at),
          expiresAt: iso(consent.expires_at),
          revokedAt: iso(consent.revoked_at),
          // 换版与撤回是两件事（docs/108 D5）。运营面把它们并成一列"已撤回"过，
          // 而那会让「这个人补签过」读起来像「这个人撤回过授权」。
          supersededAt: iso(consent.superseded_at),
          retentionState: consent.retention_state,
        }),
      ),
      grants: grants.rows.map(
        (grant): GrantSummary => ({
          id: grant.id,
          gameId: grant.game_id,
          deviceId: grant.device_id,
          status: grant.status,
          allowsTrainable: grant.allows_trainable,
          validFrom: grant.valid_from.toISOString(),
          validUntil: grant.valid_until.toISOString(),
          issuedBy: grant.issued_by,
          revokedAt: iso(grant.revoked_at),
        }),
      ),
      devices: devices.items,
      identity: identity.rows[0]
        ? {
            email: identity.rows[0].email,
            credentialStatus: identity.rows[0].credential_status,
            registeredAt: identity.rows[0].registered_at.toISOString(),
            emailVerifiedAt: iso(identity.rows[0].email_verified_at),
            lastLoginAt: iso(identity.rows[0].last_login_at),
            inviteCode: identity.rows[0].invite_code,
            invitedBy: identity.rows[0].invited_by,
            redeemedAt: iso(identity.rows[0].redeemed_at),
          }
        : null,
      profile: profile.rows[0]
        ? {
            displayName: profile.rows[0].display_name,
            phone: profile.rows[0].phone,
            imKind: profile.rows[0].im_kind,
            imHandle: profile.rows[0].im_handle,
            region: profile.rows[0].region,
            updatedAt: profile.rows[0].updated_at.toISOString(),
          }
        : null,
    };
  }

  async listDevices(query: DeviceListQuery): Promise<Page<DeviceSummary>> {
    const params = new Params();
    const filters: string[] = [];
    if (query.status) filters.push(`d.status = ${params.add(query.status)}`);
    if (query.annotatorId === unattached) filters.push(`d.annotator_id IS NULL`);
    else if (query.annotatorId) filters.push(`d.annotator_id = ${params.add(query.annotatorId)}`);
    if (query.clientVersion) filters.push(`d.client_version = ${params.add(query.clientVersion)}`);
    if (query.query) filters.push(`d.client_device_id ILIKE ${params.add(`%${query.query}%`)}`);
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(d.created_at, d.id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::uuid)`,
      );
    }
    const limit = params.add(query.pageSize + 1);

    const result = await this.#pool.query<DeviceRow>(
      `SELECT d.id, d.client_device_id, d.annotator_id, a.alias AS annotator_alias, d.status,
              d.windows_build, d.gpu_pci_id, d.gpu_driver, d.client_version,
              d.last_seen_at, d.created_at,
              roll.session_count, roll.clip_count, roll.verified_clip_count, roll.stored_bytes
         FROM devices d
         LEFT JOIN annotators a ON a.id = d.annotator_id
         LEFT JOIN LATERAL (${deviceRollup("d.id")}) roll ON TRUE
        ${where(filters)}
        ORDER BY d.created_at DESC, d.id DESC
        LIMIT ${limit}`,
      params.values,
    );

    return toPage(result.rows.map(toDeviceSummary), query.pageSize, (row) => ({
      orderedAt: row.createdAt,
      id: row.id,
    }));
  }

  async getDevice(id: string): Promise<DeviceDetail | null> {
    const result = await this.#pool.query<DeviceRow & DeviceDetailRow>(
      `SELECT d.id, d.client_device_id, d.annotator_id, a.alias AS annotator_alias, d.status,
              d.windows_build, d.gpu_pci_id, d.gpu_driver, d.client_version,
              d.last_seen_at, d.created_at, d.retired_at, d.credential_fingerprint,
              d.display_snapshot, d.input_devices_snapshot, d.certified_profile_id,
              roll.session_count, roll.clip_count, roll.verified_clip_count, roll.stored_bytes
         FROM devices d
         LEFT JOIN annotators a ON a.id = d.annotator_id
         LEFT JOIN LATERAL (${deviceRollup("d.id")}) roll ON TRUE
        WHERE d.id = $1`,
      [id],
    );
    const row = result.rows[0];
    if (!row) return null;

    return {
      ...toDeviceSummary(row),
      credentialFingerprint: row.credential_fingerprint,
      displaySnapshot: row.display_snapshot,
      inputDevicesSnapshot: row.input_devices_snapshot,
      retiredAt: iso(row.retired_at),
      certifiedProfileId: row.certified_profile_id,
    };
  }

  async listSessions(query: SessionListQuery): Promise<Page<SessionSummary>> {
    const params = new Params();
    const filters = sessionFilters(query, params, "s");
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(s.created_at, s.id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::uuid)`,
      );
    }
    const limit = params.add(query.pageSize + 1);

    const result = await this.#pool.query<SessionRow>(
      `SELECT ${sessionColumns}, roll.run_count, roll.clip_count, roll.stored_clip_count,
              roll.verified_clip_count, roll.stored_bytes
         FROM sessions s
         JOIN games g ON g.id = s.game_id
         JOIN devices d ON d.id = s.device_id
         LEFT JOIN annotators a ON a.id = s.annotator_id
         LEFT JOIN LATERAL (${sessionRollup}) roll ON TRUE
        ${where(filters)}
        ORDER BY s.created_at DESC, s.id DESC
        LIMIT ${limit}`,
      params.values,
    );

    return toPage(result.rows.map(toSessionSummary), query.pageSize, (row) => ({
      orderedAt: row.createdAt,
      id: row.id,
    }));
  }

  async getSession(id: string): Promise<SessionDetail | null> {
    const result = await this.#pool.query<SessionRow & SessionDetailRow>(
      `SELECT ${sessionColumns}, roll.run_count, roll.clip_count, roll.stored_clip_count,
              roll.verified_clip_count, roll.stored_bytes,
              s.capture_engine, s.native_width, s.native_height, s.fps_num, s.fps_den,
              s.client_version, s.game_version, s.game_executable_sha256,
              s.game_executable_sha256_missing_reason, s.manifest_sha256,
              s.catalog_id, s.catalog_version, s.catalog_manifest_sha256, s.game_profile_sha256,
              s.stop_reason, s.rejected_reason, s.sealed_at, s.consent_id, s.capture_grant_id,
              -- 设备行只钉谱系（57 §2.1），所以「这次到底跑的哪一版」只有会话行知道。
              -- 抬一次驱动地板之后，同一台机器前后两次会话在这里就是两个版本。
              dv.id AS device_profile_version_id, dv.version AS device_profile_version,
              dv.status AS device_profile_status,
              dv.approved_for_trainable AS device_profile_approved_for_trainable
         FROM sessions s
         JOIN games g ON g.id = s.game_id
         LEFT JOIN trusted_catalog_versions dv ON dv.id = s.device_catalog_version_id
         JOIN devices d ON d.id = s.device_id
         LEFT JOIN annotators a ON a.id = s.annotator_id
         LEFT JOIN LATERAL (${sessionRollup}) roll ON TRUE
        WHERE s.id = $1`,
      [id],
    );
    const row = result.rows[0];
    if (!row) return null;

    // 会话级可训练性（issue #855 PR 5）。片段详情早就逐条展示 blockers，但
    // 「这一场卡在哪」此前只能靠一条条点开数——candidate 与最终 is_trainable 之间
    // 差的是哪几道闸，本来就是 session 尺度的问题。console 不复算：数的是
    // `clip_trainability` 这个只读视图已经给出的结论。
    //
    // 该视图对 devices / games / trusted_catalog_versions 是 INNER JOIN，所以一条
    // clip 可能整行缺席（例如目录版本行被删）。LEFT JOIN + COALESCE 与 clipColumns
    // 逐字同口径：缺席按不可训练算，不按"没结论"算。
    const trainability = await this.#pool.query<SessionTrainabilityRow>(
      `SELECT count(*) FILTER (WHERE COALESCE(ct.is_trainable, false))::int AS trainable_clip_count,
              count(*) FILTER (WHERE NOT COALESCE(ct.is_trainable, false))::int AS blocked_clip_count,
              count(*) FILTER (
                WHERE c.verification_state = 'verified'
                  AND c.quality_state = 'pending'
                  AND c.retention_state = 'retained'
              )::int AS awaiting_quality_review_clip_count,
              COALESCE((
                SELECT jsonb_agg(
                         jsonb_build_object('blocker', g.blocker, 'clipCount', g.clip_count)
                         ORDER BY g.clip_count DESC, g.blocker)
                  FROM (
                    SELECT blocker::text AS blocker, count(*)::int AS clip_count
                      FROM clips c2
                      JOIN clip_trainability ct2 ON ct2.clip_id = c2.id
                      CROSS JOIN LATERAL unnest(ct2.blockers) AS blocker
                     WHERE c2.session_id = $1
                     GROUP BY blocker
                  ) g
              ), '[]'::jsonb) AS blockers
         FROM clips c
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
        WHERE c.session_id = $1`,
      [id],
    );

    // `ready_clip_count` 与 `health_json` 两列没有写入者（docs/67 §3.2），所以这里
    // 从片段侧现算：ready 段数 = 该 run 的 clip 行数，健康 = 逐段计数之和。
    const runs = await this.#pool.query<CaptureRunRow>(
      `SELECT r.id, r.run_seq, r.status, r.encoder_backend, r.width, r.height, r.fps_num, r.fps_den,
              r.frame_count, r.started_at, r.ended_at, r.stop_reason, r.error_code,
              r.preflight_schema_version, r.preflight_id, r.preflight_result,
              r.preflight_evidence_sha256,
              (SELECT count(*) FROM clips c WHERE c.capture_run_id = r.id) AS clip_count,
              (SELECT jsonb_build_object(
                        'duplicateFrames', COALESCE(sum((c.health_json ->> 'duplicateFrames')::numeric), 0),
                        'sourceSuperseded', COALESCE(sum((c.health_json ->> 'sourceSuperseded')::numeric), 0),
                        'queueDrops', COALESCE(sum((c.health_json ->> 'queueDrops')::numeric), 0),
                        'inputOverflow', COALESCE(sum((c.health_json ->> 'inputOverflow')::numeric), 0))
                 FROM clips c WHERE c.capture_run_id = r.id) AS health_json
         FROM capture_runs r WHERE r.session_id = $1 ORDER BY r.run_seq`,
      [id],
    );

    return {
      ...toSessionSummary(row),
      captureEngine: row.capture_engine,
      nativeWidth: row.native_width,
      nativeHeight: row.native_height,
      fpsNum: row.fps_num,
      fpsDen: row.fps_den,
      clientVersion: row.client_version,
      gameVersion: row.game_version,
      gameExecutableSha256: row.game_executable_sha256,
      gameExecutableSha256MissingReason: row.game_executable_sha256_missing_reason,
      manifestSha256: row.manifest_sha256,
      catalogId: row.catalog_id,
      catalogVersion: row.catalog_version,
      catalogManifestSha256: row.catalog_manifest_sha256,
      gameProfileSha256: row.game_profile_sha256,
      stopReason: row.stop_reason,
      rejectedReason: row.rejected_reason,
      sealedAt: iso(row.sealed_at),
      consentId: row.consent_id,
      captureGrantId: row.capture_grant_id,
      deviceProfile: row.device_profile_version_id
        ? {
            versionId: row.device_profile_version_id,
            version: row.device_profile_version,
            status: row.device_profile_status,
            approvedForTrainable: row.device_profile_approved_for_trainable,
          }
        : null,
      trainability: toSessionTrainability(trainability.rows[0]),
      runs: runs.rows.map(
        (run): CaptureRunSummary => ({
          id: run.id,
          runSeq: run.run_seq,
          status: run.status,
          encoderBackend: run.encoder_backend,
          width: run.width,
          height: run.height,
          fpsNum: run.fps_num,
          fpsDen: run.fps_den,
          frameCount: run.frame_count === null ? null : count(run.frame_count),
          readyClipCount: count(run.clip_count),
          startedAt: iso(run.started_at),
          endedAt: iso(run.ended_at),
          stopReason: run.stop_reason,
          errorCode: run.error_code,
          preflightSchemaVersion: run.preflight_schema_version,
          preflightId: run.preflight_id,
          preflightResult: run.preflight_result,
          preflightEvidenceSha256: run.preflight_evidence_sha256,
          health: toClipHealth(run.health_json),
        }),
      ),
    };
  }

  async listClips(query: ClipListQuery): Promise<Page<ClipSummary>> {
    const params = new Params();
    const filters = sessionFilters(query, params, "c");
    if (query.sessionId) filters.push(`c.session_id = ${params.add(query.sessionId)}`);
    if (query.ingestState) filters.push(`c.ingest_state = ${params.add(query.ingestState)}`);
    if (query.verificationState) {
      filters.push(`c.verification_state = ${params.add(query.verificationState)}`);
    }
    if (query.qualityState) filters.push(`c.quality_state = ${params.add(query.qualityState)}`);
    if (query.unhealthyOnly) filters.push(clipIsUnhealthy);
    if (query.retentionState) filters.push(`c.retention_state = ${params.add(query.retentionState)}`);
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(c.created_at, c.id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::uuid)`,
      );
    }
    const limit = params.add(query.pageSize + 1);

    const result = await this.#pool.query<ClipRow>(
      `SELECT ${clipColumns}
         FROM clips c
         JOIN sessions s ON s.id = c.session_id
         JOIN capture_runs r ON r.id = c.capture_run_id
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
        ${where(filters)}
        ORDER BY c.created_at DESC, c.id DESC
        LIMIT ${limit}`,
      params.values,
    );

    return toPage(result.rows.map(toClipSummary), query.pageSize, (row) => ({
      orderedAt: row.createdAt,
      id: row.id,
    }));
  }

  async getClip(id: string): Promise<ClipDetail | null> {
    const result = await this.#pool.query<ClipRow & ClipDetailRow>(
      `SELECT ${clipColumns}, c.revision, c.session_frame_start, c.qpc_start, c.qpc_end,
              c.manifest_sha256, c.rejected_reason, c.stored_at, c.verified_at,
              c.quality_reviewed_at, c.quality_reviewed_by, c.purge_requested_at, c.purged_at,
              c.health_json, c.meta_json, c.manifest_json, ct.blockers
         FROM clips c
         JOIN sessions s ON s.id = c.session_id
         JOIN capture_runs r ON r.id = c.capture_run_id
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
        WHERE c.id = $1`,
      [id],
    );
    const row = result.rows[0];
    if (!row) return null;

    const [artifacts, jobs] = await Promise.all([
      this.#pool.query<ArtifactRow>(
        `SELECT kind, bucket_name, object_key, expected_sha256, expected_size_bytes,
                storage_state, verification_state, stored_at, verified_at, last_error_code
           FROM artifact_objects WHERE clip_id = $1 ORDER BY kind`,
        [id],
      ),
      this.#pool.query<VerificationJobRow>(
        `SELECT v.id, ao.kind, v.status, v.attempt_count, v.last_error_code,
                v.available_at, v.updated_at
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
          WHERE ao.clip_id = $1
          ORDER BY v.updated_at DESC`,
        [id],
      ),
    ]);

    return {
      ...toClipSummary(row),
      revision: row.revision,
      sessionFrameStart: count(row.session_frame_start),
      qpcStart: String(row.qpc_start),
      qpcEnd: String(row.qpc_end),
      manifestSha256: row.manifest_sha256,
      rejectedReason: row.rejected_reason,
      storedAt: iso(row.stored_at),
      verifiedAt: iso(row.verified_at),
      qualityReviewedAt: iso(row.quality_reviewed_at),
      qualityReviewedBy: row.quality_reviewed_by,
      purgeRequestedAt: iso(row.purge_requested_at),
      purgedAt: iso(row.purged_at),
      blockers: row.blockers ?? [],
      meta: row.meta_json,
      manifest: row.manifest_json,
      artifacts: artifacts.rows.map(
        (artifact): ArtifactSummary => ({
          kind: artifact.kind,
          bucketName: artifact.bucket_name,
          objectKey: artifact.object_key,
          expectedSha256: artifact.expected_sha256,
          expectedSizeBytes: count(artifact.expected_size_bytes),
          storageState: artifact.storage_state,
          verificationState: artifact.verification_state,
          storedAt: iso(artifact.stored_at),
          verifiedAt: iso(artifact.verified_at),
          lastErrorCode: artifact.last_error_code,
        }),
      ),
      verificationJobs: jobs.rows.map(
        (job): VerificationAttempt => ({
          id: job.id,
          artifactKind: job.kind,
          status: job.status,
          attempts: job.attempt_count,
          lastErrorCode: job.last_error_code,
          availableAt: iso(job.available_at),
          updatedAt: job.updated_at.toISOString(),
        }),
      ),
    };
  }

  async listGames(): Promise<GameSummary[]> {
    const result = await this.#pool.query<GameRow>(
      `SELECT g.id, g.title, g.content_kind, g.allowed_for_capture, g.approved_for_trainable,
              (SELECT count(*) FROM sessions s WHERE s.game_id = g.id) AS session_count,
              (SELECT count(*) FROM clips c
                 JOIN sessions s ON s.id = c.session_id
                WHERE s.game_id = g.id) AS clip_count
         FROM games g
        ORDER BY g.title`,
    );
    return result.rows.map((row) => ({
      id: row.id,
      title: row.title,
      contentKind: row.content_kind,
      allowedForCapture: row.allowed_for_capture,
      approvedForTrainable: row.approved_for_trainable,
      sessionCount: count(row.session_count),
      clipCount: count(row.clip_count),
    }));
  }

  async listAuditEvents(query: AuditQuery): Promise<Page<AuditEntry>> {
    const params = new Params();
    const filters: string[] = [];
    if (query.actorType) filters.push(`actor_type = ${params.add(query.actorType)}`);
    if (query.actorId) filters.push(`actor_id = ${params.add(query.actorId)}`);
    if (query.entityType) filters.push(`entity_type = ${params.add(query.entityType)}`);
    if (query.entityId) filters.push(`entity_id = ${params.add(query.entityId)}`);
    if (query.action) filters.push(`action = ${params.add(query.action)}`);
    if (query.from) filters.push(`occurred_at >= ${params.add(query.from)}`);
    if (query.to) filters.push(`occurred_at < ${params.add(query.to)}`);
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(occurred_at, id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::bigint)`,
      );
    }
    const limit = params.add(query.pageSize + 1);

    const result = await this.#pool.query<AuditRow>(
      `SELECT id, occurred_at, request_id, actor_type, actor_id, action,
              entity_type, entity_id, details
         FROM audit_events
        ${where(filters)}
        ORDER BY occurred_at DESC, id DESC
        LIMIT ${limit}`,
      params.values,
    );

    const entries = result.rows.map(
      (row): AuditEntry => ({
        id: String(row.id),
        occurredAt: row.occurred_at.toISOString(),
        requestId: row.request_id,
        actorType: row.actor_type,
        actorId: row.actor_id,
        action: row.action,
        entityType: row.entity_type,
        entityId: row.entity_id,
        details: row.details,
      }),
    );
    return toPage(entries, query.pageSize, (row) => ({ orderedAt: row.occurredAt, id: row.id }));
  }

  async contributionForAnnotator(
    id: string,
    days: number,
    basis?: TimeBasis,
    tz?: string,
  ): Promise<Contribution> {
    return this.#contribution(
      `s.annotator_id = $1`,
      id,
      days,
      basis ?? defaultTimeBasis,
      tz ?? defaultStatsTimeZone,
    );
  }

  async contributionForDevice(
    id: string,
    days: number,
    basis?: TimeBasis,
    tz?: string,
  ): Promise<Contribution> {
    return this.#contribution(
      `s.device_id = $1`,
      id,
      days,
      basis ?? defaultTimeBasis,
      tz ?? defaultStatsTimeZone,
    );
  }

  async pipelineOverview(): Promise<PipelineOverview> {
    const result = await this.#pool.query<OverviewRow>(
      // 件数与时长必须来自**同一次扫描的同一组行**：分两条语句写，任何一侧改了
      // 谓词就会出现「82,989 段 / 却只有 3 小时」这种自相矛盾的一格，而这种错
      // 不会报警，只会被读成真事。所以片段侧的每一格都由这个 rollup 用 FILTER
      // 切出来，count 与 sum 共用同一个 WHERE。
      `WITH clip_rollup AS (
         SELECT
           count(*) AS clip_total,
           count(*) FILTER (WHERE c.ingest_state = 'pending') AS clip_pending,
           count(*) FILTER (WHERE c.ingest_state = 'uploading') AS clip_uploading,
           count(*) FILTER (WHERE c.ingest_state = 'stored') AS clip_stored,
           count(*) FILTER (WHERE c.ingest_state = 'retry_wait') AS clip_retry,
           count(*) FILTER (WHERE c.ingest_state = 'failed') AS clip_failed,
           count(*) FILTER (WHERE c.verification_state = 'verified') AS clip_verified,
           count(*) FILTER (WHERE c.verification_state = 'rejected') AS clip_rejected,
           count(*) FILTER (WHERE c.retention_state <> 'retained') AS clip_purge_active,
           count(*) FILTER (WHERE ct.is_trainable) AS trainable_clips,
           -- depth 是可选的第七件产物；件数按 clip 算（一个 clip 至多一件 depth，
           -- UNIQUE(clip_id, kind) 保证；EXISTS 让它在将来分段上传时也仍然读作
           -- 「有几条带 depth」）。
           count(*) FILTER (WHERE dep.has_depth) AS depth_clips,
           COALESCE(sum(${clipDurationSeconds}), 0) AS seconds_total,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.ingest_state = 'pending'), 0) AS seconds_pending,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.ingest_state = 'uploading'), 0) AS seconds_uploading,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.ingest_state = 'stored'), 0) AS seconds_stored,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.ingest_state = 'retry_wait'), 0) AS seconds_retry,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.ingest_state = 'failed'), 0) AS seconds_failed,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.verification_state = 'verified'), 0) AS seconds_verified,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.verification_state = 'rejected'), 0) AS seconds_rejected,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE c.retention_state <> 'retained'), 0) AS seconds_purge_active,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE ct.is_trainable), 0) AS trainable_seconds,
           COALESCE(sum(${clipDurationSeconds})
             FILTER (WHERE dep.has_depth), 0) AS depth_seconds
         FROM clips c
         -- capture_runs 是 NOT NULL 外键，所以内连接不会漏掉任何一段；fps 只有
         -- 在 run 上，没有它就算不出时长。
         JOIN capture_runs r ON r.id = c.capture_run_id
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
         LEFT JOIN LATERAL (
           SELECT true AS has_depth
             FROM artifact_objects ao
            WHERE ao.clip_id = c.id AND ao.kind = 'depth' AND ao.storage_state = 'stored'
            LIMIT 1
         ) dep ON true
       )
       SELECT
         (SELECT count(*) FROM devices) AS device_total,
         (SELECT count(*) FROM devices WHERE status = 'active') AS device_active,
         (SELECT count(*) FROM devices
           WHERE last_seen_at > transaction_timestamp() - interval '24 hours') AS device_24h,
         (SELECT count(*) FROM devices
           WHERE last_seen_at > transaction_timestamp() - interval '7 days') AS device_7d,
         (SELECT count(*) FROM sessions) AS session_total,
         (SELECT count(*) FROM sessions
           WHERE created_at > transaction_timestamp() - interval '24 hours') AS session_24h,
         (SELECT COALESCE(sum(expected_size_bytes), 0) FROM artifact_objects
           WHERE storage_state = 'stored') AS stored_bytes,
         (SELECT COALESCE(sum(expected_size_bytes), 0) FROM artifact_objects
           WHERE storage_state = 'stored' AND kind = 'depth') AS depth_bytes,
         cr.*
       FROM clip_rollup cr`,
    );
    const row = requiredRow(result.rows[0]);
    return {
      devices: {
        total: count(row.device_total),
        active: count(row.device_active),
        seenLast24h: count(row.device_24h),
        seenLast7d: count(row.device_7d),
      },
      sessions: { total: count(row.session_total), last24h: count(row.session_24h) },
      clips: {
        total: count(row.clip_total),
        pending: count(row.clip_pending),
        uploading: count(row.clip_uploading),
        stored: count(row.clip_stored),
        retryWait: count(row.clip_retry),
        failed: count(row.clip_failed),
        verified: count(row.clip_verified),
        rejected: count(row.clip_rejected),
        purgeActive: count(row.clip_purge_active),
      },
      clipSeconds: {
        total: count(row.seconds_total),
        pending: count(row.seconds_pending),
        uploading: count(row.seconds_uploading),
        stored: count(row.seconds_stored),
        retryWait: count(row.seconds_retry),
        failed: count(row.seconds_failed),
        verified: count(row.seconds_verified),
        rejected: count(row.seconds_rejected),
        purgeActive: count(row.seconds_purge_active),
      },
      storedBytes: count(row.stored_bytes),
      trainableClips: count(row.trainable_clips),
      trainableSeconds: count(row.trainable_seconds),
      depth: {
        clips: count(row.depth_clips),
        bytes: count(row.depth_bytes),
        seconds: count(row.depth_seconds),
      },
    };
  }

  async pipelineHealth(days: number, tz?: string): Promise<PipelineHealth> {
    const effectiveTz = tz ?? defaultStatsTimeZone;
    const [jobs, uploads, storage, failures, qualityBacklog] = await Promise.all([
      this.#pool.query<JobCountRow>(
        `SELECT 'verification' AS kind, status, count(*)::int AS count FROM verification_jobs GROUP BY status
          UNION ALL
         SELECT 'purge' AS kind, status, count(*)::int AS count FROM purge_jobs GROUP BY status`,
      ),
      this.#pool.query<UploadCountRow>(
        `SELECT
           (SELECT count(*) FROM upload_intents
             WHERE status IN ('pending', 'uploading', 'completing')) AS active_intents,
           (SELECT count(*) FROM multipart_uploads
             WHERE status IN ('initiated', 'uploading', 'completing')) AS active_multiparts,
           (SELECT count(*) FROM multipart_uploads
             WHERE status IN ('initiated', 'uploading', 'completing')
               AND expires_at <= transaction_timestamp()) AS expired_multiparts`,
      ),
      // 天界与每日进线/贡献面同一缺省与同一纪律（docs/97 P2、§4.2）。
      this.#pool.query<StorageDayRow>(
        `SELECT to_char(day, 'YYYY-MM-DD') AS day,
                COALESCE(agg.stored_bytes, 0) AS stored_bytes,
                COALESCE(agg.clip_count, 0) AS clip_count
           FROM generate_series(
                  date_trunc('day', transaction_timestamp() AT TIME ZONE $2)
                    - make_interval(days => $1::int - 1),
                  date_trunc('day', transaction_timestamp() AT TIME ZONE $2),
                  interval '1 day'
                ) AS day
           LEFT JOIN LATERAL (
             SELECT count(*) AS clip_count,
                    COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes
               FROM clips c
              WHERE (c.created_at AT TIME ZONE $2) >= day
                AND (c.created_at AT TIME ZONE $2) < day + interval '1 day'
           ) agg ON TRUE
          ORDER BY day`,
        [days, effectiveTz],
      ),
      this.#pool.query<FailureRow>(
        `SELECT 'verification' AS kind, ao.clip_id AS entity_id, v.last_error_code,
                v.attempt_count, v.updated_at
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
          WHERE v.status IN ('failed', 'dead_letter', 'retry_wait')
          UNION ALL
         SELECT 'purge' AS kind, p.clip_id AS entity_id, p.last_error_code,
                p.attempt_count, p.updated_at
           FROM purge_jobs p
          WHERE p.status IN ('failed', 'dead_letter', 'retry_wait') AND p.clip_id IS NOT NULL
          ORDER BY updated_at DESC
          LIMIT 20`,
      ),
      // QA 积压（issue #855 PR 5）。判定是人做的，所以这里不预测吞吐，只把真实
      // 队列摆出来。口径 = 此刻确实可以被裁决的片段：传输验证已过（与两个判定入口
      // 的 WHERE 条件一致）、仍 pending、且没有进入删除流程。等待从 `verified_at`
      // 起算——在那之前判定根本不被允许，从 `created_at` 起算会把上传和验证的时间
      // 算进人的账上。
      //
      // 阈值走参数而不是写死在 SQL 里，是为了让"超期"这个口径只有一个定义点。
      this.#pool.query<QualityBacklogRow>(
        `SELECT count(*)::int AS pending_clip_count,
                count(DISTINCT c.session_id)::int AS pending_session_count,
                min(c.verified_at) AS oldest_verified_at,
                round(
                  EXTRACT(EPOCH FROM (transaction_timestamp() - min(c.verified_at)))::numeric / 3600,
                  1
                ) AS oldest_wait_hours,
                count(*) FILTER (
                  WHERE c.verified_at <= transaction_timestamp() - make_interval(hours => $1::int)
                )::int AS overdue_clip_count,
                count(DISTINCT c.session_id) FILTER (
                  WHERE c.verified_at <= transaction_timestamp() - make_interval(hours => $1::int)
                )::int AS overdue_session_count
           FROM clips c
          WHERE c.verification_state = 'verified'
            AND c.quality_state = 'pending'
            AND c.retention_state = 'retained'`,
        [qualityReviewOverdueHours],
      ),
    ]);

    const bucket = (kind: string) => {
      const rows = jobs.rows.filter((row) => row.kind === kind);
      const of = (status: string) => count(rows.find((row) => row.status === status)?.count);
      return {
        pending: of("pending"),
        running: of("running"),
        retryWait: of("retry_wait"),
        failed: of("failed"),
        deadLetter: of("dead_letter"),
      };
    };
    const uploadRow = requiredRow(uploads.rows[0]);

    return {
      verification: bucket("verification"),
      purge: bucket("purge"),
      uploads: {
        activeIntents: count(uploadRow.active_intents),
        activeMultiparts: count(uploadRow.active_multiparts),
        expiredMultiparts: count(uploadRow.expired_multiparts),
      },
      timeZone: effectiveTz,
      storageByDay: storage.rows.map((row) => ({
        day: row.day,
        storedBytes: count(row.stored_bytes),
        clipCount: count(row.clip_count),
      })),
      recentFailures: failures.rows.map((row) => ({
        kind: row.kind,
        entityId: row.entity_id,
        errorCode: row.last_error_code,
        attempts: row.attempt_count,
        updatedAt: row.updated_at.toISOString(),
      })),
      qualityBacklog: toQualityBacklog(qualityBacklog.rows[0]),
    };
  }

  async dailyIntake(
    days: number,
    basis: TimeBasis | undefined,
    tz: string,
    groupBy?: DailyGroupBy,
  ): Promise<DailyIntake> {
    const effectiveBasis = basis ?? defaultTimeBasis;
    // 分桶列与 #contribution 同一纪律（docs/67 §3.3）：显式选择，不藏在列名里。
    const clipTime = effectiveBasis === "registered" ? "c.created_at" : clipRecordedAt;
    // 天界在 $1 时区里生成与比较（docs/97 §4.2）。`AT TIME ZONE` 产出 naive
    // timestamp，若经 Date 往返，驱动会按 UTC 再解释一次——UTC 缺省时零偏移
    // 掩盖了这条路径，换任何非零偏移时区，每个天标签都会错一天。所以 day
    // 用 to_char 文本直出，绝不回到 Date。
    const windowStart = `date_trunc('day', transaction_timestamp() AT TIME ZONE $1)
             - make_interval(days => $2::int - 1)`;
    const windowEnd = `date_trunc('day', transaction_timestamp() AT TIME ZONE $1) + interval '1 day'`;
    // trainable 是现在时视图的回看（docs/97 P2）：consent 撤销、grant 过期都会
    // 改写历史天的这个数，所以它叫「回看」而不叫「当日」。
    const metricColumns = `count(DISTINCT c.session_id) AS session_count,
                    count(*) AS clip_count,
                    count(*) FILTER (WHERE c.ingest_state = 'stored') AS stored_clip_count,
                    count(*) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
                    count(*) FILTER (WHERE ${clipIsUnhealthy}) AS unhealthy_clip_count,
                    COALESCE(sum(${clipDurationSeconds}), 0) AS captured_seconds,
                    COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes,
                    count(DISTINCT s.device_id) AS active_device_count,
                    count(DISTINCT s.annotator_id) AS active_annotator_count,
                    count(*) FILTER (WHERE ct.is_trainable) AS trainable_clip_count,
                    count(*) FILTER (WHERE ${clipHasDepth}) AS depth_clip_count,
                    COALESCE(sum(${depthBytesForClip}), 0) AS depth_bytes`;
    const metricJoins = `FROM clips c
               JOIN sessions s ON s.id = c.session_id
               JOIN capture_runs r ON r.id = c.capture_run_id
               LEFT JOIN clip_trainability ct ON ct.clip_id = c.id`;

    const [daily, totals] = await Promise.all([
      this.#pool.query<DailyIntakeDayRow>(
        `SELECT to_char(day, 'YYYY-MM-DD') AS day,
                COALESCE(agg.session_count, 0) AS session_count,
                COALESCE(agg.clip_count, 0) AS clip_count,
                COALESCE(agg.stored_clip_count, 0) AS stored_clip_count,
                COALESCE(agg.verified_clip_count, 0) AS verified_clip_count,
                COALESCE(agg.unhealthy_clip_count, 0) AS unhealthy_clip_count,
                COALESCE(agg.captured_seconds, 0) AS captured_seconds,
                COALESCE(agg.stored_bytes, 0) AS stored_bytes,
                COALESCE(agg.active_device_count, 0) AS active_device_count,
                COALESCE(agg.active_annotator_count, 0) AS active_annotator_count,
                COALESCE(agg.trainable_clip_count, 0) AS trainable_clip_count,
                COALESCE(agg.depth_clip_count, 0) AS depth_clip_count,
                COALESCE(agg.depth_bytes, 0) AS depth_bytes
           FROM generate_series(
                  ${windowStart},
                  date_trunc('day', transaction_timestamp() AT TIME ZONE $1),
                  interval '1 day'
                ) AS day
           LEFT JOIN LATERAL (
             SELECT ${metricColumns}
               ${metricJoins}
              WHERE (${clipTime} AT TIME ZONE $1) >= day
                AND (${clipTime} AT TIME ZONE $1) < day + interval '1 day'
           ) agg ON TRUE
          ORDER BY day`,
        [tz, days],
      ),
      // 合计单独查：distinct 的活跃设备/采集员按窗口整体去重，逐日行加不出来。
      this.#pool.query<DailyIntakeTotalsRow>(
        `SELECT ${metricColumns}
           ${metricJoins}
          WHERE (${clipTime} AT TIME ZONE $1) >= ${windowStart}
            AND (${clipTime} AT TIME ZONE $1) < ${windowEnd}`,
        [tz, days],
      ),
    ]);

    const counts = (row: DailyIntakeTotalsRow) => ({
      sessionCount: count(row.session_count),
      clipCount: count(row.clip_count),
      storedClipCount: count(row.stored_clip_count),
      verifiedClipCount: count(row.verified_clip_count),
      unhealthyClipCount: count(row.unhealthy_clip_count),
      capturedSeconds: count(row.captured_seconds),
      storedBytes: count(row.stored_bytes),
      activeDeviceCount: count(row.active_device_count),
      activeAnnotatorCount: count(row.active_annotator_count),
      trainableClipCount: count(row.trainable_clip_count),
      depthClipCount: count(row.depth_clip_count),
      depthBytes: count(row.depth_bytes),
    });
    const intake: DailyIntake = {
      timeBasis: effectiveBasis,
      timeZone: tz,
      daily: daily.rows.map((row): DailyIntakeDay => ({ day: row.day, ...counts(row) })),
      totals: counts(requiredRow(totals.rows[0])),
    };
    if (!groupBy) return intake;

    // 维度拆分（docs/97 §7.8）：拆分序列不补零——空天由整体轴（intake.daily）
    // 对齐；每组合计单独 GROUP BY，distinct 数逐日加不出来（同上）。
    const groupKey =
      groupBy === "game" ? "s.game_id" : `COALESCE(s.annotator_id::text, '${unattached}')`;
    const groupLabel =
      groupBy === "game"
        ? "g.title"
        : `CASE WHEN s.annotator_id IS NULL THEN '${unattached}'
                ELSE COALESCE(a.alias, s.annotator_id::text) END`;
    const groupJoin =
      groupBy === "game"
        ? "JOIN games g ON g.id = s.game_id"
        : "LEFT JOIN annotators a ON a.id = s.annotator_id";
    const [groupDaily, groupTotals] = await Promise.all([
      this.#pool.query<DailyIntakeGroupDayRow>(
        `SELECT ${groupKey} AS group_key, ${groupLabel} AS group_label,
                to_char(date_trunc('day', ${clipTime} AT TIME ZONE $1), 'YYYY-MM-DD') AS day,
                ${metricColumns}
           ${metricJoins}
           ${groupJoin}
          WHERE (${clipTime} AT TIME ZONE $1) >= ${windowStart}
            AND (${clipTime} AT TIME ZONE $1) < ${windowEnd}
          GROUP BY 1, 2, 3
          ORDER BY 1, 3`,
        [tz, days],
      ),
      this.#pool.query<DailyIntakeGroupRow>(
        `SELECT ${groupKey} AS group_key, ${groupLabel} AS group_label,
                ${metricColumns}
           ${metricJoins}
           ${groupJoin}
          WHERE (${clipTime} AT TIME ZONE $1) >= ${windowStart}
            AND (${clipTime} AT TIME ZONE $1) < ${windowEnd}
          GROUP BY 1, 2
          ORDER BY clip_count DESC, 1`,
        [tz, days],
      ),
    ]);

    intake.groupBy = groupBy;
    intake.groups = groupTotals.rows.map((group) => ({
      key: group.group_key,
      label: group.group_label,
      totals: counts(group),
      daily: groupDaily.rows
        .filter((row) => row.group_key === group.group_key)
        .map((row): DailyIntakeDay => ({ day: row.day, ...counts(row) })),
    }));
    return intake;
  }

  async #contribution(
    predicate: string,
    id: string,
    days: number,
    basis: TimeBasis,
    tz: string,
  ): Promise<Contribution> {
    // 片段的时间坐标：录制口径取所属 run 的起始时间，登记口径取入库时间。
    // 两个口径的差可以是几小时到几天（弱网重传、断网补传），所以图上按哪个分桶
    // 必须是显式选择，不能是聚合语句里一处不起眼的列名。
    const clipTime = basis === "registered" ? "c.created_at" : clipRecordedAt;
    // 天界在 $3 时区里生成与比较、day 标签 to_char 文本直出——与 dailyIntake
    // 同一纪律（docs/97 §4.2 的驱动二次解释陷阱），两页的同一天才是同一天。
    const [totals, daily] = await Promise.all([
      this.#pool.query<ContributionTotalsRow>(
        `SELECT
           (SELECT count(*) FROM sessions s WHERE ${predicate}) AS session_count,
           count(*) AS clip_count,
           count(*) FILTER (WHERE c.ingest_state = 'stored') AS stored_clip_count,
           count(*) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
           count(*) FILTER (WHERE c.verification_state = 'rejected') AS rejected_clip_count,
           COALESCE(sum(${clipDurationSeconds}), 0) AS captured_seconds,
           count(*) FILTER (WHERE ${clipIsUnhealthy}) AS unhealthy_clip_count,
           COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes,
           min(${clipTime}) AS first_activity_at,
           max(${clipTime}) AS last_activity_at
          FROM clips c
          JOIN sessions s ON s.id = c.session_id
          JOIN capture_runs r ON r.id = c.capture_run_id
         WHERE ${predicate}`,
        [id],
      ),
      this.#pool.query<ContributionDayRow>(
        `SELECT to_char(day, 'YYYY-MM-DD') AS day,
                COALESCE(agg.session_count, 0) AS session_count,
                COALESCE(agg.clip_count, 0) AS clip_count,
                COALESCE(agg.verified_clip_count, 0) AS verified_clip_count,
                COALESCE(agg.captured_seconds, 0) AS captured_seconds,
                COALESCE(agg.unhealthy_clip_count, 0) AS unhealthy_clip_count,
                COALESCE(agg.stored_bytes, 0) AS stored_bytes
           FROM generate_series(
                  date_trunc('day', transaction_timestamp() AT TIME ZONE $3)
                    - make_interval(days => $2::int - 1),
                  date_trunc('day', transaction_timestamp() AT TIME ZONE $3),
                  interval '1 day'
                ) AS day
           LEFT JOIN LATERAL (
             SELECT count(DISTINCT c.session_id) AS session_count,
                    count(*) AS clip_count,
                    count(*) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
                    COALESCE(sum(${clipDurationSeconds}), 0) AS captured_seconds,
                    count(*) FILTER (WHERE ${clipIsUnhealthy}) AS unhealthy_clip_count,
                    COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes
               FROM clips c
               JOIN sessions s ON s.id = c.session_id
               JOIN capture_runs r ON r.id = c.capture_run_id
              WHERE ${predicate}
                AND (${clipTime} AT TIME ZONE $3) >= day
                AND (${clipTime} AT TIME ZONE $3) < day + interval '1 day'
           ) agg ON TRUE
          ORDER BY day`,
        [id, days, tz],
      ),
    ]);

    const row = requiredRow(totals.rows[0]);
    return {
      timeBasis: basis,
      timeZone: tz,
      totals: {
        sessionCount: count(row.session_count),
        clipCount: count(row.clip_count),
        storedClipCount: count(row.stored_clip_count),
        verifiedClipCount: count(row.verified_clip_count),
        rejectedClipCount: count(row.rejected_clip_count),
        capturedSeconds: count(row.captured_seconds),
        unhealthyClipCount: count(row.unhealthy_clip_count),
        storedBytes: count(row.stored_bytes),
        firstActivityAt: iso(row.first_activity_at),
        lastActivityAt: iso(row.last_activity_at),
      },
      daily: daily.rows.map(
        (day): ContributionDay => ({
          day: day.day,
          sessionCount: count(day.session_count),
          clipCount: count(day.clip_count),
          verifiedClipCount: count(day.verified_clip_count),
          capturedSeconds: count(day.captured_seconds),
          unhealthyClipCount: count(day.unhealthy_clip_count),
          storedBytes: count(day.stored_bytes),
        }),
      ),
    };
  }

  async listSessionArtifacts(sessionId: string): Promise<SessionArtifactSummary[]> {
    const result = await this.#pool.query<SessionArtifactRow>(
      `SELECT kind, object_key, content_type, expected_sha256, expected_size_bytes,
              ingest_state, verification_state, stored_at, verified_at, rejected_reason
         FROM session_artifacts WHERE session_id = $1 ORDER BY kind`,
      [sessionId],
    );
    return result.rows.map((row) => ({
      kind: row.kind,
      objectKey: row.object_key,
      contentType: row.content_type,
      expectedSha256: row.expected_sha256,
      expectedSizeBytes: count(row.expected_size_bytes),
      ingestState: row.ingest_state,
      verificationState: row.verification_state,
      storedAt: iso(row.stored_at),
      verifiedAt: iso(row.verified_at),
      rejectedReason: row.rejected_reason,
    }));
  }

  async listClientEvents(query: ClientEventListQuery): Promise<Page<ClientEventEntry>> {
    const params = new Params();
    const filters: string[] = [];
    if (query.deviceId) filters.push(`e.device_id = ${params.add(query.deviceId)}`);
    if (query.annotatorId) filters.push(`e.annotator_id = ${params.add(query.annotatorId)}`);
    if (query.sessionId) filters.push(`e.session_id = ${params.add(query.sessionId)}`);
    if (query.severity) filters.push(`e.severity = ${params.add(query.severity)}`);
    if (query.kind) filters.push(`e.kind = ${params.add(query.kind)}`);
    if (query.from) filters.push(`e.occurred_at >= ${params.add(query.from)}`);
    if (query.to) filters.push(`e.occurred_at < ${params.add(query.to)}`);
    const cursor = decodeCursor(query.cursor);
    if (cursor) {
      filters.push(
        `(e.occurred_at, e.event_id) < (${params.add(cursor.orderedAt)}::timestamptz, ${params.add(cursor.id)}::uuid)`,
      );
    }
    const limit = params.add(query.pageSize + 1);
    const result = await this.#pool.query<ClientEventRow>(
      `SELECT e.event_id, e.device_id, e.annotator_id, e.session_id, e.occurred_at, e.received_at,
              e.kind, e.severity, e.client_version, e.details, e.retention_state
         FROM client_events e
        ${where(filters)}
        ORDER BY e.occurred_at DESC, e.event_id DESC
        LIMIT ${limit}`,
      params.values,
    );
    return toPage(
      result.rows.map(
        (row): ClientEventEntry => ({
          eventId: row.event_id,
          deviceId: row.device_id,
          annotatorId: row.annotator_id,
          sessionId: row.session_id,
          occurredAt: row.occurred_at.toISOString(),
          receivedAt: row.received_at.toISOString(),
          kind: row.kind,
          severity: row.severity,
          clientVersion: row.client_version,
          details: row.details,
          retentionState: row.retention_state,
        }),
      ),
      query.pageSize,
      (row) => ({ orderedAt: row.occurredAt, id: row.eventId }),
    );
  }

  /**
   * 产出报表（docs/145）：一条聚合 + 一次名单查询。
   *
   * 与 `dailyIntake` 的实现差别只有一处，但省掉的东西不少：**列轴在 TypeScript 里
   * 由 from/to 生成**，SQL 只负责把有数据的桶算出来，两边靠同一套 `to_char` 文本
   * 对齐。于是不需要 `generate_series`，也不需要 `LEFT JOIN LATERAL` 逐桶补零——
   * 顺带绕开了 docs/97 §4.2-1 那个坑（`AT TIME ZONE` 的裸 timestamp 经 Date 往返
   * 会被驱动按 UTC 再解释一次）。
   *
   * 之所以只要一条聚合：`OutputMetrics` 九个数全部可加（见其注释），行合计与列合计
   * 都是加出来的。混进任何 `count(DISTINCT …)` 都会立刻要第二、第三条查询。
   */
  async outputReport(query: StatsOutputQuery): Promise<OutputReport> {
    const basis = query.timeBasis ?? defaultTimeBasis;
    const clipTime = basis === "registered" ? "c.created_at" : clipRecordedAt;
    const isMonth = query.bucket === "month";
    const params = new Params();
    const tz = params.add(query.tz);
    const from = params.add(query.from);
    const to = params.add(query.to);
    const truncUnit = isMonth ? "month" : "day";
    const labelFormat = isMonth ? "YYYY-MM" : "YYYY-MM-DD";

    const filters = [
      `(${clipTime} AT TIME ZONE ${tz}) >= ${from}::date`,
      `(${clipTime} AT TIME ZONE ${tz}) < ${to}::date + interval '1 day'`,
    ];
    // `none` 走 IS NULL 而不是等值：匿名会话没有 id，它是一个可筛的状态。
    if (query.annotatorId.length > 0) {
      const clauses = query.annotatorId.map((value) =>
        value === unattached
          ? "s.annotator_id IS NULL"
          : `s.annotator_id = ${params.add(value)}::uuid`,
      );
      filters.push(`(${clauses.join(" OR ")})`);
    }
    if (query.gameId.length > 0) {
      filters.push(`s.game_id IN (${query.gameId.map((value) => params.add(value)).join(", ")})`);
    }

    // 两条属性、两个分支。为还没决定要做的第三条属性建一张注册表，是给不存在的
    // 需求先付代价；真加第三条时，这里各加一支即可。
    const keyExpression = (dimension: OutputDimension): string =>
      dimension === "annotator" ? `COALESCE(s.annotator_id::text, '${unattached}')` : "s.game_id";
    const labelExpression = (dimension: OutputDimension): string =>
      dimension === "annotator"
        ? `CASE WHEN s.annotator_id IS NULL THEN '${unattached}'
                ELSE COALESCE(a.alias, s.annotator_id::text) END`
        : "COALESCE(g.title, s.game_id)";

    const grouped = query.groupBy.flatMap((dimension, index) => [
      `${keyExpression(dimension)} AS key_${index}`,
      `${labelExpression(dimension)} AS label_${index}`,
    ]);
    const groupByOrdinals = Array.from({ length: grouped.length + 1 }, (_, i) => i + 1).join(", ");

    const grid = await this.#pool.query<Record<string, string | number | null>>(
      `SELECT ${grouped.join(",\n              ")},
              to_char(date_trunc('${truncUnit}', ${clipTime} AT TIME ZONE ${tz}), '${labelFormat}') AS bucket,
              count(*) AS clip_count,
              count(*) FILTER (WHERE c.ingest_state = 'stored') AS stored_clip_count,
              count(*) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
              count(*) FILTER (WHERE ${clipIsUnhealthy}) AS unhealthy_clip_count,
              COALESCE(sum(${clipDurationSeconds}), 0) AS captured_seconds,
              COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes,
              count(*) FILTER (WHERE ${clipHasDepth}) AS depth_clip_count,
              COALESCE(sum(${depthBytesForClip}), 0) AS depth_bytes,
              count(*) FILTER (WHERE ct.is_trainable) AS trainable_clip_count
         FROM clips c
         JOIN sessions s ON s.id = c.session_id
         JOIN capture_runs r ON r.id = c.capture_run_id
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
         LEFT JOIN annotators a ON a.id = s.annotator_id
         LEFT JOIN games g ON g.id = s.game_id
        WHERE ${filters.join("\n          AND ")}
        GROUP BY ${groupByOrdinals}`,
      params.values,
    );

    const axis = outputAxis(query.from, query.to, query.bucket);
    const positionOf = new Map(axis.map((bucket, index) => [bucket, index]));
    const rows = new Map<string, OutputRow>();
    for (const row of grid.rows) {
      const at = positionOf.get(String(row.bucket ?? ""));
      if (at === undefined) continue;
      const keys = query.groupBy.map((_, index) => String(row[`key_${index}`] ?? unattached));
      const labels = query.groupBy.map((_, index) => String(row[`label_${index}`] ?? ""));
      const id = JSON.stringify(keys);
      let entry = rows.get(id);
      if (!entry) {
        entry = { keys, labels, totals: zeroMetrics(), cells: axis.map(() => zeroMetrics()) };
        rows.set(id, entry);
      }
      const cell = entry.cells[at];
      if (!cell) continue;
      const measured = readMetrics(row);
      addMetrics(cell, measured);
      addMetrics(entry.totals, measured);
    }

    // 补齐在册名单：空格必须意味着「他真的没交」，而从片段聚合出来的行做不出这个
    // 断言——全勤和从没开工在结果里长得一模一样，都是查无此人。只补 active：
    // 停用的人本就不该被期待有产出；但停用前交过的数据仍然会从聚合里带出自己的行。
    let enrolledAnnotatorCount: number | null = null;
    if (query.groupBy[0] === "annotator") {
      // 名单跟着筛选收窄，否则筛掉两个人之后界面仍然显示「1 / 3 人有产出」。
      // 只筛 `none` 时名单为空：问的是匿名会话，没有哪个在册的人该被补进这张表。
      const wanted = query.annotatorId.filter((value) => value !== unattached);
      const rosterParams = new Params();
      const rosterFilters = ["status = 'active'"];
      if (query.annotatorId.length > 0) {
        rosterFilters.push(
          wanted.length > 0
            ? `id IN (${wanted.map((value) => `${rosterParams.add(value)}::uuid`).join(", ")})`
            : "false",
        );
      }
      const roster = await this.#pool.query<{ id: string; alias: string | null }>(
        `SELECT id::text AS id, alias FROM annotators WHERE ${rosterFilters.join(" AND ")}`,
        rosterParams.values,
      );
      enrolledAnnotatorCount = roster.rows.length;
      const present = new Set([...rows.values()].map((row) => row.keys[0]));
      for (const person of roster.rows) {
        if (present.has(person.id)) continue;
        // 后续维度留空：他没交，也就没有游戏可归。界面把空串渲染成「—」。
        const keys = query.groupBy.map((_, index) => (index === 0 ? person.id : ""));
        rows.set(JSON.stringify(keys), {
          keys,
          labels: query.groupBy.map((_, index) => (index === 0 ? (person.alias ?? person.id) : "")),
          totals: zeroMetrics(),
          cells: axis.map(() => zeroMetrics()),
        });
      }
    }

    const ordered = [...rows.values()].sort(
      (left, right) =>
        right.totals.capturedSeconds - left.totals.capturedSeconds ||
        left.labels.join().localeCompare(right.labels.join()),
    );
    const columnTotals = axis.map(() => zeroMetrics());
    const totals = zeroMetrics();
    for (const row of ordered) {
      row.cells.forEach((cell, index) => {
        const column = columnTotals[index];
        if (column) addMetrics(column, cell);
      });
      addMetrics(totals, row.totals);
    }

    return {
      from: query.from,
      to: query.to,
      bucket: query.bucket,
      timeBasis: basis,
      timeZone: query.tz,
      groupBy: query.groupBy,
      buckets: axis,
      rows: ordered,
      columnTotals,
      totals,
      enrolledAnnotatorCount,
    };
  }

  /**
   * 游戏分布普查（docs/153）：一条 `GROUP BY` 出全表，无参数。
   *
   * 存量口径（stock），与每日进线的 flow 口径**刻意分叉**：已购删（purged）的段
   * 不再是存量，这里剔掉；purge 申请中/进行中的段数据还在，仍计。会话数在这里
   * 可加——game 划分 session——所以合计不需要第二条查询，把行加起来即可。
   */
  async gameDistribution(): Promise<GameDistribution> {
    const result = await this.#pool.query<{
      game_id: string;
      title: string;
      captured_seconds: string | number;
      session_count: string | number;
      clip_count: string | number;
      trainable_clip_count: string | number;
      trainable_seconds: string | number;
    }>(
      `SELECT s.game_id,
              COALESCE(g.title, s.game_id) AS title,
              COALESCE(sum(${clipDurationSeconds}), 0) AS captured_seconds,
              count(DISTINCT c.session_id) AS session_count,
              count(*) AS clip_count,
              count(*) FILTER (WHERE ct.is_trainable) AS trainable_clip_count,
              COALESCE(sum(${clipDurationSeconds}) FILTER (WHERE ct.is_trainable), 0) AS trainable_seconds
         FROM clips c
         JOIN sessions s ON s.id = c.session_id
         JOIN capture_runs r ON r.id = c.capture_run_id
         LEFT JOIN clip_trainability ct ON ct.clip_id = c.id
         LEFT JOIN games g ON g.id = s.game_id
        WHERE c.retention_state <> 'purged'
        GROUP BY 1, 2
        ORDER BY captured_seconds DESC, s.game_id`,
    );
    const rows = result.rows.map(
      (row): GameDistributionRow => ({
        gameId: row.game_id,
        title: row.title,
        capturedSeconds: count(row.captured_seconds),
        sessionCount: count(row.session_count),
        clipCount: count(row.clip_count),
        trainableClipCount: count(row.trainable_clip_count),
        trainableSeconds: count(row.trainable_seconds),
      }),
    );
    const totals = {
      capturedSeconds: 0,
      sessionCount: 0,
      clipCount: 0,
      trainableClipCount: 0,
      trainableSeconds: 0,
    };
    for (const row of rows) {
      totals.capturedSeconds += row.capturedSeconds;
      totals.sessionCount += row.sessionCount;
      totals.clipCount += row.clipCount;
      totals.trainableClipCount += row.trainableClipCount;
      totals.trainableSeconds += row.trainableSeconds;
    }
    return { asOf: new Date().toISOString(), rows, totals };
  }
}

/**
 * 列轴。与 SQL 里 `to_char(date_trunc(…))` 产出同形的标签，两边靠字符串对齐——
 * 这也是这条实现不需要 `generate_series` 的原因。区间已由契约限长，循环有界。
 */
function outputAxis(from: string, to: string, bucket: "day" | "month"): string[] {
  const axis: string[] = [];
  if (bucket === "day") {
    const last = Date.parse(`${to}T00:00:00Z`);
    for (let at = Date.parse(`${from}T00:00:00Z`); at <= last; at += 86_400_000) {
      axis.push(new Date(at).toISOString().slice(0, 10));
    }
    return axis;
  }
  const last = to.slice(0, 7);
  let year = Number(from.slice(0, 4));
  let month = Number(from.slice(5, 7));
  for (;;) {
    const label = `${year}-${String(month).padStart(2, "0")}`;
    axis.push(label);
    if (label >= last) return axis;
    month += 1;
    if (month > 12) {
      month = 1;
      year += 1;
    }
  }
}

const metricKeys = [
  "clipCount",
  "storedClipCount",
  "verifiedClipCount",
  "unhealthyClipCount",
  "capturedSeconds",
  "storedBytes",
  "depthClipCount",
  "depthBytes",
  "trainableClipCount",
] as const;

function zeroMetrics(): OutputMetrics {
  return {
    clipCount: 0,
    storedClipCount: 0,
    verifiedClipCount: 0,
    unhealthyClipCount: 0,
    capturedSeconds: 0,
    storedBytes: 0,
    depthClipCount: 0,
    depthBytes: 0,
    trainableClipCount: 0,
  };
}

function readMetrics(row: Record<string, unknown>): OutputMetrics {
  return {
    clipCount: count(row.clip_count),
    storedClipCount: count(row.stored_clip_count),
    verifiedClipCount: count(row.verified_clip_count),
    unhealthyClipCount: count(row.unhealthy_clip_count),
    capturedSeconds: count(row.captured_seconds),
    storedBytes: count(row.stored_bytes),
    depthClipCount: count(row.depth_clip_count),
    depthBytes: count(row.depth_bytes),
    trainableClipCount: count(row.trainable_clip_count),
  };
}

/** 就地累加。九个数全部可加，所以这一个函数同时服务行合计、列合计与总计。 */
function addMetrics(into: OutputMetrics, from: OutputMetrics): void {
  for (const key of metricKeys) into[key] += from[key];
}

// A single-row aggregate always returns exactly one row; the guard exists so the
// absence of one is a loud failure rather than a field-by-field undefined.
function requiredRow<T>(row: T | undefined): T {
  if (!row) throw new Error("aggregate query returned no row");
  return row;
}

function where(filters: string[]): string {
  return filters.length > 0 ? `WHERE ${filters.join(" AND ")}` : "";
}

/**
 * 身份维度永远在 sessions 上；时间窗按调用方分页的那张表算，且口径由
 * `timeBasis` 决定（docs/67 §3.3）：`recorded` 用录制墙钟（会话起始 / 片段所属
 * run 的起始），`registered` 用服务端登记时间。
 */
function timeColumn(alias: "s" | "c", basis: TimeBasis | undefined): string {
  if (basis === "registered") return `${alias}.created_at`;
  return alias === "s" ? "s.started_at" : clipRecordedAt;
}

function sessionFilters(
  query: SessionListQuery | ClipListQuery,
  params: Params,
  timeAlias: "s" | "c",
): string[] {
  const filters: string[] = [];
  if (query.gameId) filters.push(`s.game_id = ${params.add(query.gameId)}`);
  if (query.deviceId) filters.push(`s.device_id = ${params.add(query.deviceId)}`);
  if (query.annotatorId === unattached) filters.push(`s.annotator_id IS NULL`);
  else if (query.annotatorId) {
    filters.push(`s.annotator_id = ${params.add(query.annotatorId)}`);
  }
  if ("status" in query && query.status) filters.push(`s.status = ${params.add(query.status)}`);
  if ("catalogMode" in query && query.catalogMode) {
    filters.push(`s.catalog_mode = ${params.add(query.catalogMode)}`);
  }
  const time = timeColumn(timeAlias, query.timeBasis);
  if (query.from) filters.push(`${time} >= ${params.add(query.from)}`);
  if (query.to) filters.push(`${time} < ${params.add(query.to)}`);
  return filters;
}

function annotatorRollup(idExpression: string): string {
  return `SELECT count(DISTINCT s.id) AS session_count,
                 max(s.created_at) AS last_activity_at,
                 count(c.id) AS clip_count,
                 count(c.id) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
                 COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes
            FROM sessions s
            LEFT JOIN clips c ON c.session_id = s.id
           WHERE s.annotator_id = ${idExpression}`;
}

function deviceRollup(idExpression: string): string {
  return `SELECT count(DISTINCT s.id) AS session_count,
                 count(c.id) AS clip_count,
                 count(c.id) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
                 COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes
            FROM sessions s
            LEFT JOIN clips c ON c.session_id = s.id
           WHERE s.device_id = ${idExpression}`;
}

const sessionRollup = `SELECT
    (SELECT count(*) FROM capture_runs r WHERE r.session_id = s.id) AS run_count,
    count(c.id) AS clip_count,
    count(c.id) FILTER (WHERE c.ingest_state = 'stored') AS stored_clip_count,
    count(c.id) FILTER (WHERE c.verification_state = 'verified') AS verified_clip_count,
    COALESCE(sum(${storedBytesForClip}), 0) AS stored_bytes
  FROM clips c WHERE c.session_id = s.id`;

const sessionColumns = `s.id, s.client_session_id, s.game_id, g.title AS game_title,
       s.device_id, d.client_device_id, s.annotator_id, a.alias AS annotator_alias,
       s.status, s.server_data_classification, s.schema_version, s.catalog_mode,
       s.started_at, s.ended_at, s.created_at`;

const clipColumns = `c.id, c.session_id, s.game_id, s.device_id, s.annotator_id,
       c.run_seq, c.segment_seq, c.n_frames, c.event_count,
       c.ingest_state, c.verification_state, c.quality_state, c.retention_state,
       c.retention_deadline, c.created_at, c.health_json,
       ${clipRecordedAt} AS recorded_at,
       ${storedBytesForClip} AS stored_bytes,
       CASE WHEN r.fps_num > 0
            THEN round(c.n_frames::numeric * r.fps_den / r.fps_num, 3)
            ELSE NULL END AS duration_seconds,
       COALESCE(ct.is_trainable, false) AS is_trainable,
       COALESCE(cardinality(ct.blockers), 0) AS blocker_count`;

type Numeric = string | number | null;

type AnnotatorRow = {
  id: string;
  alias: string | null;
  status: string;
  device_count: Numeric;
  active_consent_count: Numeric;
  active_grant_count: Numeric;
  session_count: Numeric;
  clip_count: Numeric;
  verified_clip_count: Numeric;
  stored_bytes: Numeric;
  last_activity_at: Date | null;
  created_at: Date;
};

type DeviceRow = {
  id: string;
  client_device_id: string;
  annotator_id: string | null;
  annotator_alias: string | null;
  status: string;
  windows_build: string;
  gpu_pci_id: string;
  gpu_driver: string;
  client_version: string;
  last_seen_at: Date | null;
  session_count: Numeric;
  clip_count: Numeric;
  verified_clip_count: Numeric;
  stored_bytes: Numeric;
  created_at: Date;
};

type SessionRow = {
  id: string;
  client_session_id: string;
  game_id: string;
  game_title: string;
  device_id: string;
  client_device_id: string;
  annotator_id: string | null;
  annotator_alias: string | null;
  status: string;
  server_data_classification: string;
  schema_version: string;
  catalog_mode: string | null;
  started_at: Date;
  ended_at: Date | null;
  run_count: Numeric;
  clip_count: Numeric;
  stored_clip_count: Numeric;
  verified_clip_count: Numeric;
  stored_bytes: Numeric;
  created_at: Date;
};

type ConsentRow = {
  id: string;
  policy_version: string;
  purpose: string;
  status: string;
  allows_trainable_use: boolean;
  scopes: string[] | null;
  accepted_at: Date | null;
  expires_at: Date | null;
  revoked_at: Date | null;
  superseded_at: Date | null;
  retention_state: string;
};

type GrantRow = {
  id: string;
  game_id: string;
  device_id: string;
  status: string;
  allows_trainable: boolean;
  valid_from: Date;
  valid_until: Date;
  issued_by: string;
  revoked_at: Date | null;
};

type CaptureRunRow = {
  id: string;
  run_seq: number;
  status: string;
  encoder_backend: string;
  width: number;
  height: number;
  fps_num: number;
  fps_den: number;
  frame_count: Numeric;
  clip_count: Numeric;
  started_at: Date | null;
  ended_at: Date | null;
  stop_reason: string | null;
  error_code: string | null;
  preflight_schema_version: string | null;
  preflight_id: string | null;
  preflight_result: string | null;
  preflight_evidence_sha256: string | null;
  health_json: unknown;
};

type ArtifactRow = {
  kind: string;
  bucket_name: string;
  object_key: string;
  expected_sha256: string;
  expected_size_bytes: Numeric;
  storage_state: string;
  verification_state: string;
  stored_at: Date | null;
  verified_at: Date | null;
  last_error_code: string | null;
};

type VerificationJobRow = {
  id: string;
  kind: string;
  status: string;
  attempt_count: number;
  last_error_code: string | null;
  available_at: Date | null;
  updated_at: Date;
};

type GameRow = {
  id: string;
  title: string;
  content_kind: string;
  allowed_for_capture: boolean;
  approved_for_trainable: boolean;
  session_count: Numeric;
  clip_count: Numeric;
};

type AuditRow = {
  id: string | number;
  occurred_at: Date;
  request_id: string | null;
  actor_type: string;
  actor_id: string | null;
  action: string;
  entity_type: string;
  entity_id: string;
  details: unknown;
};

type OverviewRow = Record<
  | "device_total"
  | "device_active"
  | "device_24h"
  | "device_7d"
  | "session_total"
  | "session_24h"
  | "clip_total"
  | "clip_pending"
  | "clip_uploading"
  | "clip_stored"
  | "clip_retry"
  | "clip_failed"
  | "clip_verified"
  | "clip_rejected"
  | "clip_purge_active"
  | "seconds_total"
  | "seconds_pending"
  | "seconds_uploading"
  | "seconds_stored"
  | "seconds_retry"
  | "seconds_failed"
  | "seconds_verified"
  | "seconds_rejected"
  | "seconds_purge_active"
  | "stored_bytes"
  | "trainable_clips"
  | "trainable_seconds"
  | "depth_clips"
  | "depth_bytes"
  | "depth_seconds",
  Numeric
>;

type ContributionTotalsRow = {
  session_count: Numeric;
  clip_count: Numeric;
  stored_clip_count: Numeric;
  verified_clip_count: Numeric;
  rejected_clip_count: Numeric;
  captured_seconds: Numeric;
  unhealthy_clip_count: Numeric;
  stored_bytes: Numeric;
  first_activity_at: Date | null;
  last_activity_at: Date | null;
};

type SessionArtifactRow = {
  kind: string;
  object_key: string;
  content_type: string;
  expected_sha256: string;
  expected_size_bytes: Numeric;
  ingest_state: string;
  verification_state: string;
  stored_at: Date | null;
  verified_at: Date | null;
  rejected_reason: string | null;
};

type ClientEventRow = {
  event_id: string;
  device_id: string;
  annotator_id: string | null;
  session_id: string | null;
  occurred_at: Date;
  received_at: Date;
  kind: string;
  severity: string;
  client_version: string;
  details: unknown;
  retention_state: string;
};

type JobCountRow = { kind: string; status: string; count: Numeric };
type UploadCountRow = {
  active_intents: Numeric;
  active_multiparts: Numeric;
  expired_multiparts: Numeric;
};
type StorageDayRow = { day: string; stored_bytes: Numeric; clip_count: Numeric };
type FailureRow = {
  kind: "verification" | "purge";
  entity_id: string;
  last_error_code: string | null;
  attempt_count: number;
  updated_at: Date;
};

type QualityBacklogRow = {
  pending_clip_count: Numeric;
  pending_session_count: Numeric;
  oldest_verified_at: Date | null;
  /** numeric 经驱动回来是字符串，别按 number 用。 */
  oldest_wait_hours: string | number | null;
  overdue_clip_count: Numeric;
  overdue_session_count: Numeric;
};

// day 是 to_char 直出的文本，不是 Date（docs/97 §4.2 的驱动二次解释陷阱）。
type ContributionDayRow = {
  day: string;
  session_count: Numeric;
  clip_count: Numeric;
  verified_clip_count: Numeric;
  captured_seconds: Numeric;
  unhealthy_clip_count: Numeric;
  stored_bytes: Numeric;
};

// day 是 to_char 直出的文本，不是 Date（docs/97 §4.2 的驱动二次解释陷阱）。
type DailyIntakeTotalsRow = {
  session_count: Numeric;
  clip_count: Numeric;
  stored_clip_count: Numeric;
  verified_clip_count: Numeric;
  unhealthy_clip_count: Numeric;
  captured_seconds: Numeric;
  stored_bytes: Numeric;
  active_device_count: Numeric;
  active_annotator_count: Numeric;
  trainable_clip_count: Numeric;
  depth_clip_count: Numeric;
  depth_bytes: Numeric;
};
type DailyIntakeDayRow = DailyIntakeTotalsRow & { day: string };
type DailyIntakeGroupRow = DailyIntakeTotalsRow & { group_key: string; group_label: string };
type DailyIntakeGroupDayRow = DailyIntakeGroupRow & { day: string };

type AnnotatorDetailRow = {
  external_subject_hash: string | null;
  tombstoned_at: Date | null;
};

type DeviceDetailRow = {
  credential_fingerprint: string;
  display_snapshot: unknown;
  input_devices_snapshot: unknown;
  retired_at: Date | null;
  certified_profile_id: string | null;
};

type SessionDetailRow = {
  capture_engine: string;
  native_width: number;
  native_height: number;
  fps_num: number;
  fps_den: number;
  client_version: string;
  game_version: string;
  game_executable_sha256: string | null;
  game_executable_sha256_missing_reason: string | null;
  manifest_sha256: string;
  catalog_id: string | null;
  catalog_version: string | null;
  catalog_manifest_sha256: string | null;
  game_profile_sha256: string | null;
  stop_reason: string | null;
  rejected_reason: string | null;
  sealed_at: Date | null;
  consent_id: string | null;
  capture_grant_id: string | null;
  device_profile_version_id: string | null;
  device_profile_version: string;
  device_profile_status: string;
  device_profile_approved_for_trainable: boolean;
};

type SessionTrainabilityRow = {
  trainable_clip_count: Numeric;
  blocked_clip_count: Numeric;
  awaiting_quality_review_clip_count: Numeric;
  blockers: unknown;
};

type ClipDetailRow = {
  revision: number;
  session_frame_start: Numeric;
  qpc_start: string | number;
  qpc_end: string | number;
  manifest_sha256: string;
  rejected_reason: string | null;
  stored_at: Date | null;
  verified_at: Date | null;
  quality_reviewed_at: Date | null;
  quality_reviewed_by: string | null;
  purge_requested_at: Date | null;
  purged_at: Date | null;
  health_json: unknown;
  meta_json: unknown;
  manifest_json: unknown;
  blockers: string[] | null;
};

type ClipRow = {
  id: string;
  session_id: string;
  game_id: string;
  device_id: string;
  annotator_id: string | null;
  run_seq: number;
  segment_seq: number;
  n_frames: Numeric;
  event_count: Numeric;
  ingest_state: string;
  verification_state: string;
  quality_state: string;
  retention_state: string;
  retention_deadline: Date;
  created_at: Date;
  health_json: unknown;
  recorded_at: Date | null;
  stored_bytes: Numeric;
  duration_seconds: Numeric;
  is_trainable: boolean;
  blocker_count: Numeric;
};

function toAnnotatorSummary(row: AnnotatorRow): AnnotatorSummary {
  return {
    id: row.id,
    alias: row.alias,
    status: row.status,
    deviceCount: count(row.device_count),
    activeConsentCount: count(row.active_consent_count),
    activeGrantCount: count(row.active_grant_count),
    sessionCount: count(row.session_count),
    clipCount: count(row.clip_count),
    verifiedClipCount: count(row.verified_clip_count),
    storedBytes: count(row.stored_bytes),
    lastActivityAt: iso(row.last_activity_at),
    createdAt: row.created_at.toISOString(),
  };
}

function toDeviceSummary(row: DeviceRow): DeviceSummary {
  return {
    id: row.id,
    clientDeviceId: row.client_device_id,
    annotatorId: row.annotator_id,
    annotatorAlias: row.annotator_alias,
    status: row.status,
    windowsBuild: row.windows_build,
    gpuPciId: row.gpu_pci_id,
    gpuDriver: row.gpu_driver,
    clientVersion: row.client_version,
    lastSeenAt: iso(row.last_seen_at),
    sessionCount: count(row.session_count),
    clipCount: count(row.clip_count),
    verifiedClipCount: count(row.verified_clip_count),
    storedBytes: count(row.stored_bytes),
    createdAt: row.created_at.toISOString(),
  };
}

function toSessionSummary(row: SessionRow): SessionSummary {
  return {
    id: row.id,
    clientSessionId: row.client_session_id,
    gameId: row.game_id,
    gameTitle: row.game_title,
    deviceId: row.device_id,
    clientDeviceId: row.client_device_id,
    annotatorId: row.annotator_id,
    annotatorAlias: row.annotator_alias,
    status: row.status,
    serverDataClassification: row.server_data_classification,
    schemaVersion: row.schema_version,
    catalogMode: row.catalog_mode,
    startedAt: row.started_at.toISOString(),
    endedAt: iso(row.ended_at),
    runCount: count(row.run_count),
    clipCount: count(row.clip_count),
    storedClipCount: count(row.stored_clip_count),
    verifiedClipCount: count(row.verified_clip_count),
    storedBytes: count(row.stored_bytes),
    createdAt: row.created_at.toISOString(),
  };
}

function toClipSummary(row: ClipRow): ClipSummary {
  return {
    id: row.id,
    sessionId: row.session_id,
    gameId: row.game_id,
    deviceId: row.device_id,
    annotatorId: row.annotator_id,
    runSeq: row.run_seq,
    segmentSeq: row.segment_seq,
    nFrames: count(row.n_frames),
    durationSeconds: row.duration_seconds === null ? null : Number(row.duration_seconds),
    eventCount: count(row.event_count),
    ingestState: row.ingest_state,
    verificationState: row.verification_state,
    qualityState: row.quality_state,
    retentionState: row.retention_state,
    sizeBytes: count(row.stored_bytes),
    retentionDeadline: row.retention_deadline.toISOString(),
    isTrainable: row.is_trainable,
    blockerCount: count(row.blocker_count),
    health: toClipHealth(row.health_json),
    recordedAt: iso(row.recorded_at),
    createdAt: row.created_at.toISOString(),
  };
}

function toQualityBacklog(row: QualityBacklogRow | undefined): QualityBacklog {
  const oldestVerifiedAt = iso(row?.oldest_verified_at ?? null);
  return {
    pendingClipCount: count(row?.pending_clip_count),
    pendingSessionCount: count(row?.pending_session_count),
    oldestVerifiedAt,
    // 队列为空时 min()/EXTRACT 都是 NULL，别让 count() 把它折成 0——「没有人在等」
    // 和「最老的等了 0 小时」是两件事。
    oldestWaitHours: oldestVerifiedAt === null ? null : count(row?.oldest_wait_hours),
    overdueThresholdHours: qualityReviewOverdueHours,
    overdueClipCount: count(row?.overdue_clip_count),
    overdueSessionCount: count(row?.overdue_session_count),
  };
}

/**
 * 会话级可训练性汇总（issue #855 PR 5）。会话一条 clip 都没有时聚合行仍然回一行
 * 全零，所以这里的 undefined 分支只是类型上的守卫。
 */
function toSessionTrainability(row: SessionTrainabilityRow | undefined): SessionTrainability {
  const blockers = Array.isArray(row?.blockers) ? row.blockers : [];
  return {
    trainableClipCount: count(row?.trainable_clip_count),
    blockedClipCount: count(row?.blocked_clip_count),
    awaitingQualityReviewClipCount: count(row?.awaiting_quality_review_clip_count),
    blockers: blockers.map((entry) => {
      const record = (entry ?? {}) as Record<string, unknown>;
      return { blocker: String(record.blocker ?? ""), clipCount: count(record.clipCount) };
    }),
  };
}

/**
 * `health_json` 是客户端申报的对象，服务端只做过 schema 校验就整块存下。读回时
 * 逐字段防御性取数：老片段可能整块为 `{}`，而 UI 想要的是四个数，不是 `unknown`。
 */
function toClipHealth(value: unknown): ClipHealth {
  const source = (value ?? {}) as Record<string, unknown>;
  const at = (field: string): number => {
    const raw = source[field];
    return typeof raw === "number" && Number.isFinite(raw) ? raw : 0;
  };
  return {
    duplicateFrames: at("duplicateFrames"),
    sourceSuperseded: at("sourceSuperseded"),
    queueDrops: at("queueDrops"),
    inputOverflow: at("inputOverflow"),
  };
}
