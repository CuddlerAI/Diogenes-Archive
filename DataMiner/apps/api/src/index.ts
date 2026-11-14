import { buildApp } from "./app.js";
import { admittedClassifications, loadConfig } from "./config.js";
import { PostgresRepository } from "./repository.js";
import { R2ObjectStorage } from "./storage.js";

const config = loadConfig();
const repository = new PostgresRepository(
  config.databaseUrl,
  config.retentionDays,
  admittedClassifications(config),
);
const storage = new R2ObjectStorage(config.r2);
const app = buildApp({
  config,
  repository,
  storage,
  logger: true,
});

async function shutdown(signal: string) {
  app.log.info({ signal }, "shutting down");
  await app.close();
  await repository.close();
  process.exit(0);
}

process.once("SIGINT", () => void shutdown("SIGINT"));
process.once("SIGTERM", () => void shutdown("SIGTERM"));

await app.listen({ host: config.host, port: config.port });
