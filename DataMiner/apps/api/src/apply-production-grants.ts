import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Client } from "pg";

const target = process.env.DATAMINER_ENV;
if (target !== "production") {
  throw new Error("DATAMINER_ENV must be exactly production");
}
if (process.env.CONFIRM_DATABASE_GRANTS !== "apply-production-runtime-grants") {
  throw new Error(
    "CONFIRM_DATABASE_GRANTS must be exactly apply-production-runtime-grants",
  );
}

for (const name of ["PGHOST", "PGDATABASE", "PGUSER", "PGPASSWORD"] as const) {
  if (!process.env[name]) throw new Error(`${name} is required`);
}
if (process.env.PGDATABASE !== "dataminer_production") {
  throw new Error("PGDATABASE must be exactly dataminer_production");
}

const grantDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../db-grants",
);
const files = ["grant-runtime-role.sql", "grant-console-roles.sql"] as const;
const requiredRoles = [
  "dataminer_api",
  "dataminer_console",
  "dataminer_app",
  "dataminer_catalog_publisher",
] as const;

const client = new Client();
await client.connect();
try {
  const database = await client.query<{ database: string; user_name: string }>(
    "SELECT current_database() AS database, current_user AS user_name",
  );
  if (database.rows[0]?.database !== "dataminer_production") {
    throw new Error("connected database is not dataminer_production");
  }

  const roles = await client.query<{ rolname: string }>(
    "SELECT rolname FROM pg_roles WHERE rolname = ANY($1::text[])",
    [requiredRoles],
  );
  const present = new Set(roles.rows.map((row) => row.rolname));
  const missing = requiredRoles.filter((role) => !present.has(role));
  if (missing.length > 0) {
    throw new Error(`required production roles are missing: ${missing.join(", ")}`);
  }

  for (const file of files) {
    process.stdout.write(`applying ${file}\n`);
    await client.query(await readFile(path.join(grantDirectory, file), "utf8"));
  }
  process.stdout.write(
    `${JSON.stringify({ database: database.rows[0].database, applied: files })}\n`,
  );
} finally {
  await client.end();
}
