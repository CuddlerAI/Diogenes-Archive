import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

type TablePrivilege = "SELECT" | "INSERT" | "UPDATE" | "DELETE";

const sourceDirectory = path.dirname(fileURLToPath(import.meta.url));
const grantFile = path.resolve(sourceDirectory, "../scripts/grant-runtime-role.sql");
const servingSqlFiles = [
  "repository.ts",
  "client-events-export.ts",
  "catalog-delivery.ts",
] as const;

function stripComments(value: string): string {
  return value.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/--.*$/gm, " ");
}

function parseRuntimeGrants(sql: string): Map<string, Set<TablePrivilege>> {
  const grants = new Map<string, Set<TablePrivilege>>();
  const withoutComments = stripComments(sql);
  const statement =
    /GRANT\s+((?:SELECT|INSERT|UPDATE|DELETE)(?:\s*\([^)]*\))?(?:\s*,\s*(?:SELECT|INSERT|UPDATE|DELETE)(?:\s*\([^)]*\))?)*)\s+ON\s+([\s\S]*?)\s+TO\s+dataminer_api\s*;/gi;

  for (const match of withoutComments.matchAll(statement)) {
    const privileges = match[1]
      ?.split(",")
      .map(
        (privilege) =>
          privilege.trim().replace(/\s*\([^)]*\)$/, "").toUpperCase() as TablePrivilege,
      );
    const tables = match[2]?.split(",").map((table) => table.trim());
    if (!privileges || !tables) continue;

    for (const table of tables) {
      if (!/^[a-z_][a-z0-9_]*$/i.test(table)) continue;
      const tableGrants = grants.get(table) ?? new Set<TablePrivilege>();
      for (const privilege of privileges) tableGrants.add(privilege);
      grants.set(table, tableGrants);
    }
  }

  return grants;
}

function extractSqlTemplates(source: string): string[] {
  const templates: string[] = [];
  const withoutTypeScriptComments = source
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/^\s*\/\/.*$/gm, " ");
  for (const match of withoutTypeScriptComments.matchAll(/`((?:\\.|[^`])*)`/gs)) {
    if (match[1]) templates.push(stripComments(match[1]));
  }
  return templates;
}

function requiredPrivileges(sql: string): Array<readonly [string, TablePrivilege]> {
  const ctes = new Set(
    [...sql.matchAll(/\b(?:WITH|,)\s*([a-z_][a-z0-9_]*)\s+AS\s*(?:NOT\s+MATERIALIZED\s+|MATERIALIZED\s+)?\(/gi)]
      .map((match) => match[1]?.toLowerCase())
      .filter((name): name is string => Boolean(name)),
  );
  const required: Array<readonly [string, TablePrivilege]> = [];
  const sqlKeywords = new Set(["of", "set", "skip"]);
  const reference = /\b(INSERT\s+INTO|DELETE\s+FROM|UPDATE|FROM|JOIN)\s+([a-z_][a-z0-9_]*)/gi;

  for (const match of sql.matchAll(reference)) {
    const operation = match[1]?.toUpperCase().replace(/\s+/g, " ");
    const table = match[2]?.toLowerCase();
    if (!operation || !table || ctes.has(table) || table === "lateral" || sqlKeywords.has(table)) {
      continue;
    }

    const afterReference = sql.slice((match.index ?? 0) + match[0].length);
    if (/^\s*\(/.test(afterReference)) continue; // set-returning function, not a table

    const privilege: TablePrivilege =
      operation === "INSERT INTO"
        ? "INSERT"
        : operation === "DELETE FROM"
          ? "DELETE"
          : operation === "UPDATE"
            ? "UPDATE"
            : "SELECT";
    required.push([table, privilege]);
  }

  return required;
}

describe("dataminer_api runtime grants", () => {
  it("cover every table operation issued by the serving database paths", async () => {
    const grants = parseRuntimeGrants(await readFile(grantFile, "utf8"));
    const missing = new Set<string>();

    for (const file of servingSqlFiles) {
      const source = await readFile(path.join(sourceDirectory, file), "utf8");
      for (const sql of extractSqlTemplates(source)) {
        for (const [table, privilege] of requiredPrivileges(sql)) {
          if (!grants.get(table)?.has(privilege)) {
            missing.add(`${table}.${privilege} (${file})`);
          }
        }
      }
    }

    expect([...missing].sort()).toEqual([]);
  });

  it("keeps session artifact evidence writable but undeletable", async () => {
    const sql = await readFile(grantFile, "utf8");
    const grants = parseRuntimeGrants(sql);
    expect([...grants.get("session_artifacts") ?? []].sort()).toEqual([
      "INSERT",
      "SELECT",
      "UPDATE",
    ]);
  });

  it("keeps the device heartbeat write column-scoped", async () => {
    const sql = stripComments(await readFile(grantFile, "utf8"));
    const repository = await readFile(path.join(sourceDirectory, "repository.ts"), "utf8");
    const deviceUpdates = extractSqlTemplates(repository).filter((template) =>
      /\bUPDATE\s+devices\b/i.test(template),
    );

    expect(deviceUpdates).toHaveLength(1);
    expect(deviceUpdates[0]).toMatch(/\bUPDATE\s+devices\s+SET\s+last_seen_at\b/i);
    expect(sql).toMatch(
      /GRANT\s+UPDATE\s*\(\s*last_seen_at\s*\)\s+ON\s+devices\s+TO\s+dataminer_api\s*;/i,
    );
    expect(sql).not.toMatch(/GRANT\s+UPDATE\s+ON\s+devices\s+TO\s+dataminer_api\s*;/i);
  });
});
