import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";

import type { D1Database, D1PreparedStatement } from "../d1.js";

/**
 * 用 node:sqlite 实现 D1 接口，让**生产 SQL 原样**跑在真实 SQLite 上。
 *
 * D1 本身就是 SQLite，所以这里验证的 json_each() 展开、upsert 守卫、STRICT 表约束
 * 和 ON CONFLICT 语义都是真的会在 D1 上发生的事——不是一个「返回预设值」的 mock。
 * 它验证不了的：D1 的网络错误、单线程排队、绑定参数上限（那些靠契约层限制批大小来避免）。
 */

const MIGRATION_PATH = fileURLToPath(new URL("../../migrations/0001_mirror_schema.sql", import.meta.url));

class SqliteStatement implements D1PreparedStatement {
  readonly #db: DatabaseSync;
  readonly #sql: string;
  readonly #values: unknown[];

  constructor(db: DatabaseSync, sql: string, values: unknown[] = []) {
    this.#db = db;
    this.#sql = sql;
    this.#values = values;
  }

  bind(...values: unknown[]): D1PreparedStatement {
    return new SqliteStatement(this.#db, this.#sql, values);
  }

  execute(): void {
    this.#db.prepare(this.#sql).run(this.#indexedBindings());
  }

  async run(): Promise<unknown> {
    this.execute();
    return { success: true };
  }

  async all<T = Record<string, unknown>>(): Promise<{ results: T[] }> {
    return { results: this.#db.prepare(this.#sql).all(this.#indexedBindings()) as T[] };
  }

  async first<T = Record<string, unknown>>(): Promise<T | null> {
    const row = this.#db.prepare(this.#sql).get(this.#indexedBindings());
    return (row as T | undefined) ?? null;
  }

  /**
   * D1 SQL uses SQLite's numbered placeholders (`?1`, `?2`, ...), including
   * repeated references to the same value. Node 22's `node:sqlite` treats a
   * spread array as anonymous `?` bindings and rejects that SQL with
   * `column index out of range`; an indexed binding object preserves the real
   * D1 statement and its reuse semantics.
   */
  #indexedBindings(): Record<string, null | number | bigint | string | Uint8Array> {
    return Object.fromEntries(
      this.#normalized().map((value, index) => [String(index + 1), value]),
    );
  }

  /** node:sqlite 只接受 null/number/bigint/string/Uint8Array，undefined 要显式转 null。 */
  #normalized(): (null | number | bigint | string | Uint8Array)[] {
    return this.#values.map((value) => {
      if (value === undefined || value === null) return null;
      if (typeof value === "boolean") return value ? 1 : 0;
      if (
        typeof value === "number" ||
        typeof value === "bigint" ||
        typeof value === "string" ||
        value instanceof Uint8Array
      ) {
        return value;
      }
      throw new TypeError(`unsupported bind value: ${typeof value}`);
    });
  }
}

export class SqliteD1 implements D1Database {
  readonly #db: DatabaseSync;

  private constructor(db: DatabaseSync) {
    this.#db = db;
  }

  static createWithSchema(): SqliteD1 {
    const db = new DatabaseSync(":memory:");
    db.exec(readFileSync(MIGRATION_PATH, "utf8"));
    return new SqliteD1(db);
  }

  prepare(query: string): D1PreparedStatement {
    return new SqliteStatement(this.#db, query);
  }

  /** D1 把一个 batch 当作单个隐式事务；这里用真实事务复现该语义。 */
  async batch(statements: D1PreparedStatement[]): Promise<unknown[]> {
    this.#db.exec("BEGIN");
    try {
      for (const statement of statements) {
        (statement as SqliteStatement).execute();
      }
      this.#db.exec("COMMIT");
    } catch (error) {
      this.#db.exec("ROLLBACK");
      throw error;
    }
    return statements.map(() => ({ success: true }));
  }

  /** 测试断言用的直查通道，不属于 D1 接口。 */
  query<T = Record<string, unknown>>(sql: string, ...values: unknown[]): T[] {
    return this.#db.prepare(sql).all(...(values as never[])) as T[];
  }

  /**
   * 播种一行凭据镜像。生产上这张表由 console 在签发/吊销凭据时写入；
   * 本服务自己**没有**写它的路径，所以测试也只能从外部塞。
   */
  seedCredential(input: {
    fingerprint: string;
    deviceId: string;
    deviceStatus?: string;
    annotatorStatus?: string;
    sourceSeq?: number;
  }): void {
    this.#db
      .prepare(
        `INSERT INTO mirror_device_credentials
           (credential_fingerprint, device_id, source_seq, device_status, annotator_status, updated_at)
         VALUES (?, ?, ?, ?, ?, 0)`,
      )
      .run(
        input.fingerprint,
        input.deviceId,
        input.sourceSeq ?? 1,
        input.deviceStatus ?? "active",
        input.annotatorStatus ?? "unbound",
      );
  }
}
