/**
 * D1 的最小结构化子集。
 *
 * 刻意不依赖 `@cloudflare/workers-types`：只声明这个 Worker 真正用到的四个方法，
 * 测试就能用 node:sqlite 提供一个同形状的实现，让**生产 SQL 原样**跑在真实 SQLite 上，
 * 而不是跑在一个假装自己是数据库的 mock 上。
 */

export interface D1PreparedStatement {
  bind(...values: unknown[]): D1PreparedStatement;
  run(): Promise<unknown>;
  all<T = Record<string, unknown>>(): Promise<{ results: T[] }>;
  first<T = Record<string, unknown>>(): Promise<T | null>;
}

export interface D1Database {
  prepare(query: string): D1PreparedStatement;
  /** D1 把一个 batch 当作单个隐式事务执行。 */
  batch(statements: D1PreparedStatement[]): Promise<unknown[]>;
}
