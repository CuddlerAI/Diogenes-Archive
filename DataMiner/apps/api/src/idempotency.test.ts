import { describe, expect, it, vi } from "vitest";

import type { IdempotencyResponse, IngestionRepository } from "./domain.js";
import { runIdempotent, runResignableIdempotent } from "./idempotency.js";

type StoredEntry = {
  requestHash: string;
  response?: IdempotencyResponse;
};

function inMemoryIdempotencyRepository() {
  const entries = new Map<string, StoredEntry>();
  const identity = (scope: string, key: string) => `${scope}\u0000${key}`;
  const finishIdempotency = vi.fn(
    async (scope: string, key: string, response: IdempotencyResponse) => {
      const entry = entries.get(identity(scope, key));
      if (!entry) throw new Error("reservation missing");
      entry.response = response;
    },
  );
  const releaseIdempotency = vi.fn(async (scope: string, key: string, requestHash: string) => {
    const id = identity(scope, key);
    if (entries.get(id)?.requestHash === requestHash) entries.delete(id);
  });
  const repository = {
    getIdempotency: async (scope: string, key: string) => entries.get(identity(scope, key)) ?? null,
    reserveIdempotency: async (scope: string, key: string, requestHash: string) => {
      const id = identity(scope, key);
      if (entries.has(id)) return "exists" as const;
      entries.set(id, { requestHash });
      return "reserved" as const;
    },
    finishIdempotency,
    releaseIdempotency,
  } as unknown as IngestionRepository;
  return { repository, entries, finishIdempotency, releaseIdempotency };
}

describe("idempotency", () => {
  it("rejects reuse of one key with a different request body", async () => {
    const { repository } = inMemoryIdempotencyRepository();
    await runIdempotent(repository, "device:put:clip", "request-key", { revision: 1 }, async () => ({
      statusCode: 200,
      body: { clipId: "clip-1" },
    }));
    const secondOperation = vi.fn();

    await expect(
      runIdempotent(
        repository,
        "device:put:clip",
        "request-key",
        { revision: 2 },
        secondOperation,
      ),
    ).rejects.toMatchObject({ code: "idempotency_key_reused", statusCode: 409 });
    expect(secondOperation).not.toHaveBeenCalled();
  });

  it("replays stable response headers together with the body", async () => {
    const { repository } = inMemoryIdempotencyRepository();
    const operation = vi.fn(async () => ({
      statusCode: 201,
      headers: { location: "/v1/sessions/session-1" },
      body: { sessionId: "session-1" },
    }));
    await runIdempotent(repository, "device:create-session", "header-key", { local: "session-1" }, operation);

    const replay = await runIdempotent(
      repository,
      "device:create-session",
      "header-key",
      { local: "session-1" },
      operation,
    );

    expect(replay.replayed).toBe(true);
    expect(replay.headers).toEqual({ location: "/v1/sessions/session-1" });
    expect(operation).toHaveBeenCalledTimes(1);
  });

  it("re-signs on replay and never persists a presigned bearer URL", async () => {
    const { repository, entries, finishIdempotency } = inMemoryIdempotencyRepository();
    const operation = vi
      .fn<() => Promise<{ url: string }>>()
      .mockResolvedValueOnce({ url: "https://r2.example/first-secret" })
      .mockResolvedValueOnce({ url: "https://r2.example/second-secret" });

    const first = await runResignableIdempotent(
      repository,
      "device:sign-parts",
      "sign-key",
      { artifactType: "video", partNumber: 1 },
      operation,
    );
    const replay = await runResignableIdempotent(
      repository,
      "device:sign-parts",
      "sign-key",
      { artifactType: "video", partNumber: 1 },
      operation,
    );

    expect(first.value.url).toContain("first-secret");
    expect(first.replayed).toBe(false);
    expect(replay.value.url).toContain("second-secret");
    expect(replay.replayed).toBe(true);
    expect(operation).toHaveBeenCalledTimes(2);
    expect(finishIdempotency).toHaveBeenCalledTimes(1);
    expect(finishIdempotency).toHaveBeenCalledWith("device:sign-parts", "sign-key", {
      statusCode: 204,
      body: { resignable: true },
    });
    expect(JSON.stringify([...entries.values()])).not.toContain("r2.example");
  });

  it("keeps the reservation when persisting a successful business result fails", async () => {
    const { repository, entries, finishIdempotency, releaseIdempotency } = inMemoryIdempotencyRepository();
    finishIdempotency.mockRejectedValueOnce(new Error("database unavailable"));
    const operation = vi.fn(async () => ({
      statusCode: 201,
      headers: { location: "/v1/sessions/session-1" },
      body: { sessionId: "session-1" },
    }));

    await expect(
      runIdempotent(repository, "device:create-session", "durable-key", { localSessionId: "local-1" }, operation),
    ).rejects.toThrow("database unavailable");

    expect(operation).toHaveBeenCalledTimes(1);
    expect(releaseIdempotency).not.toHaveBeenCalled();
    expect(entries.size).toBe(1);
    await expect(
      runIdempotent(repository, "device:create-session", "durable-key", { localSessionId: "local-1" }, operation),
    ).rejects.toMatchObject({ code: "request_in_progress", statusCode: 409 });
    expect(operation).toHaveBeenCalledTimes(1);
  });
});
