import { randomUUID } from "node:crypto";

import type { ConsoleConfig } from "./config.js";
import { badRequest, conflict, notFound } from "./errors.js";
import type { SqlClient } from "./sql.js";

// HF release reconciliation (docs/33 §6). R2 and HF are DISJOINT sets by
// design (the upload gate skips consented sessions — docs/18 §3.2), so this is
// bookkeeping, not a pipeline: `not_released` is the expected state for every
// R2 object, and a match is a signal for a human to double-check. Nothing here
// feeds any admission or compliance decision.

export type HfReleaseItem = {
  repoId: string;
  path: string;
  revision: string;
  sizeBytes: number | null;
  sessionKey: string | null;
  sampleKey: string | null;
};

export type HfReconcileResult = {
  repoId: string;
  revision: string;
  itemCount: number;
  matchableCount: number;
  checkedAt: string;
};

export type HfManualMark = {
  id: string;
  source: string;
  subjectKey: string;
  mark: string;
  note: string | null;
  operatorId: string;
  createdAt: string;
};

export type HfSubjectStatus = {
  /** Computed from the cached repo listing. `mismatch` = a path matched but
   * the byte size the caller knows contradicts the listing. */
  computed: "released" | "not_released" | "mismatch";
  matches: HfReleaseItem[];
  manualMark: HfManualMark | null;
  /** Newest reconcile timestamp across configured repos; null = never ran. */
  checkedAt: string | null;
};

export interface HfClient {
  /** Resolve the current revision (commit sha) of a dataset repo. */
  resolveRevision(repoId: string): Promise<string>;
  /** Full recursive file listing at a revision: path + size. */
  listFiles(repoId: string, revision: string): Promise<{ path: string; sizeBytes: number | null }[]>;
}

/** Thin fetch wrapper over the HF Hub HTTP API (read-only token). No SDK: two
 * endpoints, bearer auth, Link-header pagination. */
export class HttpHfClient implements HfClient {
  readonly #token: string;
  readonly #base: string;

  constructor(token: string, base = "https://huggingface.co") {
    this.#token = token;
    this.#base = base;
  }

  async #get(url: string): Promise<Response> {
    const response = await fetch(url, {
      headers: { authorization: `Bearer ${this.#token}` },
      signal: AbortSignal.timeout(30_000),
    });
    if (response.status === 401 || response.status === 403) {
      throw conflict("hf_unauthorized", "the HF token was refused for this repo");
    }
    if (response.status === 404) {
      throw notFound("hf_repo_not_found", "the HF repo or revision does not exist");
    }
    if (!response.ok) {
      throw conflict("hf_unavailable", `the HF Hub answered ${response.status}`);
    }
    return response;
  }

  async resolveRevision(repoId: string): Promise<string> {
    const response = await this.#get(`${this.#base}/api/datasets/${repoId}`);
    const body = (await response.json()) as { sha?: string };
    if (!body.sha) throw conflict("hf_unavailable", "the HF Hub answered without a revision sha");
    return body.sha;
  }

  async listFiles(
    repoId: string,
    revision: string,
  ): Promise<{ path: string; sizeBytes: number | null }[]> {
    const files: { path: string; sizeBytes: number | null }[] = [];
    let url: string | null =
      `${this.#base}/api/datasets/${repoId}/tree/${revision}?recursive=true&limit=1000`;
    while (url) {
      const response = await this.#get(url);
      const entries = (await response.json()) as { type: string; path: string; size?: number }[];
      for (const entry of entries) {
        if (entry.type === "file") {
          files.push({ path: entry.path, sizeBytes: entry.size ?? null });
        }
      }
      url = nextLink(response.headers.get("link"));
    }
    return files;
  }
}

function nextLink(header: string | null): string | null {
  if (!header) return null;
  const match = /<([^>]+)>;\s*rel="next"/.exec(header);
  return match?.[1] ?? null;
}

// Teaser layout: {split}/{game_slug}/{session}/cNNNN/… (docs/18 §6). The
// matching keys are the session directory name and `<session>_cNNNN`. Paths
// that do not fit the layout (README, index/, tools/) simply carry no keys.
export function matchKeysOf(path: string): { sessionKey: string | null; sampleKey: string | null } {
  const segments = path.split("/");
  const chunkIndex = segments.findIndex((segment) => /^c\d{4}$/.test(segment));
  if (chunkIndex >= 1) {
    const session = segments[chunkIndex - 1]!;
    return { sessionKey: session, sampleKey: `${session}_${segments[chunkIndex]!}` };
  }
  if (segments.length >= 3) {
    // {split}/{game_slug}/{session}/<file> — a session-level file.
    return { sessionKey: segments[2] ?? null, sampleKey: null };
  }
  return { sessionKey: null, sampleKey: null };
}

// A review subject key (unit directory prefix) yields the same two keys from
// the other side: RAW `…/non-trainable-sessions/<session>/runs/…/segments/<n>/`
// matches by session; dm-1 `…/<session>/cNNNN/` matches by session and sample.
export function subjectMatchKeys(subjectKey: string): { sessions: string[]; samples: string[] } {
  const segments = subjectKey.split("/").filter((segment) => segment.length > 0);
  const sessions: string[] = [];
  const samples: string[] = [];
  for (let index = 0; index < segments.length; index += 1) {
    const segment = segments[index]!;
    if (/^c\d{4}$/.test(segment) && index >= 1) {
      samples.push(`${segments[index - 1]!}_${segment}`);
    }
    if (segments[index - 1] === "non-trainable-sessions" || segments[index - 1] === "sessions") {
      sessions.push(segment);
    }
  }
  // dm-1 layout has no marker directory; treat the chunk's parent as a session
  // candidate too. Matching is exact-name only — no fuzzing (docs/33 §6.1).
  for (let index = 0; index < segments.length; index += 1) {
    if (/^c\d{4}$/.test(segments[index]!) && index >= 1) {
      sessions.push(segments[index - 1]!);
    }
  }
  return { sessions: [...new Set(sessions)], samples: [...new Set(samples)] };
}

type ItemRow = {
  repo_id: string;
  path: string;
  revision: string;
  size_bytes: string | number | null;
  session_key: string | null;
  sample_key: string | null;
};

type MarkRow = {
  id: string;
  source: string;
  subject_key: string;
  mark: string;
  note: string | null;
  operator_id: string;
  created_at: Date;
};

export class HfService {
  readonly #pool: SqlClient;
  readonly #client: HfClient | null;
  readonly #config: ConsoleConfig;

  constructor(pool: SqlClient, client: HfClient | null, config: ConsoleConfig) {
    this.#pool = pool;
    this.#client = client;
    this.#config = config;
  }

  get available(): boolean {
    return this.#client !== null && this.#config.hf !== null;
  }

  repos(): string[] {
    return this.#config.hf?.repos ?? [];
  }

  async reconcile(repoId: string, operatorId: string): Promise<HfReconcileResult> {
    const hf = this.#config.hf;
    if (!this.#client || !hf) {
      throw conflict("hf_not_configured", "this deployment has no HF token, so reconciliation is unavailable");
    }
    // Hard allowlist: the token is never steered at a repo the deployment did
    // not name (docs/33 §6.2).
    if (!hf.repos.includes(repoId)) {
      throw badRequest("hf_repo_not_allowlisted", "the repo is not in CONSOLE_HF_REPOS");
    }

    const revision = await this.#client.resolveRevision(repoId);
    const files = await this.#client.listFiles(repoId, revision);
    let matchable = 0;

    // Whole-repo replace, as ONE statement. `SqlClient` is a plain pool —
    // every query autocommits on its own connection — so a DELETE followed by
    // an INSERT loop is not a transaction: a failure mid-loop would leave a
    // half-written snapshot, and a concurrent reader would see an empty table
    // while the loop runs. A single DELETE+INSERT CTE is atomic by statement,
    // the same discipline as recordClientEvent's outbox write (docs/25 §5).
    const paths: string[] = [];
    const sizes: (number | null)[] = [];
    const sessionKeys: (string | null)[] = [];
    const sampleKeys: (string | null)[] = [];
    for (const file of files) {
      const keys = matchKeysOf(file.path);
      if (keys.sessionKey || keys.sampleKey) matchable += 1;
      paths.push(file.path);
      sizes.push(file.sizeBytes);
      sessionKeys.push(keys.sessionKey);
      sampleKeys.push(keys.sampleKey);
    }
    // Upsert what the revision holds, then delete what it no longer holds.
    // (A delete-then-reinsert CTE cannot work here: sub-statements share one
    // snapshot, so re-inserting a surviving path trips the primary key.)
    // The two write sets are disjoint — updated rows are IN the listing,
    // deleted rows are NOT IN it — so the statement never touches a row twice.
    await this.#pool.query(
      `WITH incoming AS (
         SELECT u.path, u.size_bytes, u.session_key, u.sample_key
           FROM unnest($3::text[], $4::bigint[], $5::text[], $6::text[])
                AS u(path, size_bytes, session_key, sample_key)
       ),
       upserted AS (
         INSERT INTO hf_release_items(repo_id, path, revision, size_bytes, session_key, sample_key)
         SELECT $1, path, $2, size_bytes, session_key, sample_key FROM incoming
         ON CONFLICT (repo_id, path) DO UPDATE
           SET revision = EXCLUDED.revision,
               size_bytes = EXCLUDED.size_bytes,
               session_key = EXCLUDED.session_key,
               sample_key = EXCLUDED.sample_key
       )
       DELETE FROM hf_release_items
        WHERE repo_id = $1
          AND path NOT IN (SELECT path FROM incoming)`,
      [repoId, revision, paths, sizes, sessionKeys, sampleKeys],
    );
    const check = await this.#pool.query<{ checked_at: Date }>(
      `INSERT INTO hf_release_checks(id, repo_id, revision, operator_id, stats)
       VALUES ($1, $2, $3, $4, $5)
       RETURNING checked_at`,
      [
        randomUUID(),
        repoId,
        revision,
        operatorId,
        JSON.stringify({ itemCount: files.length, matchableCount: matchable }),
      ],
    );
    return {
      repoId,
      revision,
      itemCount: files.length,
      matchableCount: matchable,
      checkedAt: check.rows[0]!.checked_at.toISOString(),
    };
  }

  async status(
    source: string,
    subjectKey: string,
    videoSizeBytes: number | null,
  ): Promise<HfSubjectStatus> {
    const { sessions, samples } = subjectMatchKeys(subjectKey);
    const matches =
      sessions.length === 0 && samples.length === 0
        ? { rows: [] as ItemRow[] }
        : await this.#pool.query<ItemRow>(
            `SELECT repo_id, path, revision, size_bytes, session_key, sample_key
               FROM hf_release_items
              WHERE session_key = ANY($1::text[]) OR sample_key = ANY($2::text[])
              ORDER BY path
              LIMIT 200`,
            [sessions, samples],
          );

    const items = matches.rows.map((row) => ({
      repoId: row.repo_id,
      path: row.path,
      revision: row.revision,
      sizeBytes: row.size_bytes === null ? null : Number(row.size_bytes),
      sessionKey: row.session_key,
      sampleKey: row.sample_key,
    }));

    // `mismatch` needs a byte fact from the caller's side of the fence: the
    // reviewer knows the unit's video size from the tree listing. A matched
    // p0.mp4 whose size disagrees means a same-named release carries different
    // bytes — exactly the two-sets red line's sentinel (docs/33 §6.1).
    let computed: HfSubjectStatus["computed"] = items.length > 0 ? "released" : "not_released";
    if (videoSizeBytes !== null) {
      const video = items.find((item) => item.path.endsWith("p0.mp4") && item.sampleKey !== null);
      if (video && video.sizeBytes !== null && video.sizeBytes !== videoSizeBytes) {
        computed = "mismatch";
      }
    }

    const marks = await this.#pool.query<MarkRow>(
      `SELECT id, source, subject_key, mark, note, operator_id, created_at
         FROM hf_manual_marks
        WHERE source = $1 AND subject_key = $2
        ORDER BY created_at DESC, id DESC
        LIMIT 1`,
      [source, subjectKey],
    );
    const mark = marks.rows[0];

    const checks = await this.#pool.query<{ checked_at: Date }>(
      `SELECT max(checked_at) AS checked_at FROM hf_release_checks`,
    );

    return {
      computed,
      matches: items,
      manualMark: mark
        ? {
            id: mark.id,
            source: mark.source,
            subjectKey: mark.subject_key,
            mark: mark.mark,
            note: mark.note,
            operatorId: mark.operator_id,
            createdAt: mark.created_at.toISOString(),
          }
        : null,
      checkedAt: checks.rows[0]?.checked_at?.toISOString() ?? null,
    };
  }

  async recordMark(
    source: string,
    subjectKey: string,
    mark: string,
    note: string | null,
    operatorId: string,
  ): Promise<HfManualMark> {
    const result = await this.#pool.query<MarkRow>(
      `INSERT INTO hf_manual_marks(id, source, subject_key, mark, note, operator_id)
       VALUES ($1, $2, $3, $4, $5, $6)
       RETURNING id, source, subject_key, mark, note, operator_id, created_at`,
      [randomUUID(), source, subjectKey, mark, note, operatorId],
    );
    const row = result.rows[0]!;
    return {
      id: row.id,
      source: row.source,
      subjectKey: row.subject_key,
      mark: row.mark,
      note: row.note,
      operatorId: row.operator_id,
      createdAt: row.created_at.toISOString(),
    };
  }
}
