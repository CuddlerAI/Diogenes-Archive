/**
 * CLI for the approved-build allowlist (migration 0018).
 *
 * 指纹不再是 authoring 硬门（#278），把关改在这里：客户端实测的
 * executable SHA-256 必须在名单上且 approved，clip 才可能可训练。
 *
 * Usage (through the Cloud SQL proxy for production):
 *
 *   # 看有哪些新 build 在等审批
 *   DATABASE_URL=... pnpm --filter @dataminer/api approve:executable -- --list --status pending
 *
 *   # 批准一个（先自己核对来源：这个 build 是不是官方更新）
 *   DATABASE_URL=... pnpm --filter @dataminer/api approve:executable -- \
 *     --game halo-mcc-steam --sha256 sha256:<64hex> \
 *     --approve --approver "<operator email>" --note "Steam build 1234567"
 *
 *   # 撤销（其后续 clip 立即失去训练资格；已撤销的不会被例行批准复活）
 *   DATABASE_URL=... pnpm --filter @dataminer/api approve:executable -- \
 *     --game halo-mcc-steam --sha256 sha256:<64hex> --revoke --approver "<operator email>"
 */
import pg from "pg";

import {
  approveExecutable,
  listExecutables,
  revokeExecutable,
  type ExecutableStatus,
} from "../src/executable-approval.js";

const { Client } = pg;

function flag(name: string): boolean {
  return process.argv.includes(name);
}

function option(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  if (index === -1) return undefined;
  const value = process.argv[index + 1];
  if (!value || value.startsWith("--")) {
    console.error(`${name} requires a value`);
    process.exit(2);
  }
  return value;
}

if (!process.env.DATABASE_URL) {
  console.error("DATABASE_URL is required");
  process.exit(2);
}

const wantsList = flag("--list");
const wantsApprove = flag("--approve");
const wantsRevoke = flag("--revoke");
const selected = [wantsList, wantsApprove, wantsRevoke].filter(Boolean).length;
if (selected !== 1) {
  console.error("exactly one of --list / --approve / --revoke is required");
  process.exit(2);
}

const gameId = option("--game");
const sha256 = option("--sha256");
const approver = option("--approver");
const note = option("--note");
const status = option("--status") as ExecutableStatus | undefined;
if (status !== undefined && !["pending", "approved", "revoked"].includes(status)) {
  console.error("--status must be pending, approved, or revoked");
  process.exit(2);
}

const client = new Client({ connectionString: process.env.DATABASE_URL });
await client.connect();
const query = (text: string, params?: readonly unknown[]) =>
  client.query(text, params as unknown[]);
try {
  if (wantsList) {
    const rows = await listExecutables(query, {
      ...(gameId !== undefined ? { gameId } : {}),
      ...(status !== undefined ? { status } : {}),
    });
    console.log(JSON.stringify(rows, null, 2));
    console.log(`${rows.length} row(s)`);
  } else {
    if (!gameId || !sha256) {
      console.error("--game and --sha256 are required for --approve / --revoke");
      process.exit(2);
    }
    if (!approver) {
      console.error("--approver <name> is required so the decision is attributable");
      process.exit(2);
    }
    const result = wantsApprove
      ? await approveExecutable(query, {
          gameId,
          sha256,
          approver,
          ...(note !== undefined ? { note } : {}),
        })
      : await revokeExecutable(query, {
          gameId,
          sha256,
          actor: approver,
          ...(note !== undefined ? { note } : {}),
        });
    console.log(JSON.stringify(result, null, 2));
    if (result.action === "not_found") {
      console.error(
        "✘ no allowlist row: the build must have been authored in a registered catalog " +
          "or observed in a real capture session first",
      );
      process.exitCode = 1;
    }
    if (result.action === "refused_revoked") {
      console.error("✘ refused: this build was revoked; reinstating it is an explicit governance act");
      process.exitCode = 1;
    }
  }
} catch (error) {
  console.error("✘ failed:", error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
} finally {
  await client.end();
}
