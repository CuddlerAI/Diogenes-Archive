import { Pool } from "pg";

/**
 * The repositories depend on this shape rather than on `pg.Pool` directly.
 *
 * Only `rows` is used: driver-specific metadata such as `rowCount` is
 * deliberately avoided, so "did this statement affect a row" is always expressed
 * as `RETURNING` plus a row count that any PostgreSQL client reports the same
 * way. That also lets the whole repository be exercised against an embedded
 * engine in tests instead of being argued from the SQL text.
 */
export type SqlClient = {
  query<T>(text: string, values?: unknown[]): Promise<{ rows: T[] }>;
};

export function createPool(databaseUrl: string, maxConnections = 8): Pool {
  return new Pool({ connectionString: databaseUrl, max: maxConnections });
}
