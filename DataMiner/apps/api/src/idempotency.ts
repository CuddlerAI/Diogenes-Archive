import { stableJsonSha256 } from "./checksum.js";
import type { IdempotencyResponse, IngestionRepository } from "./domain.js";
import { ApiError, conflict } from "./errors.js";

export async function runIdempotent(
  repository: IngestionRepository,
  scope: string,
  key: string,
  request: unknown,
  operation: () => Promise<IdempotencyResponse>,
): Promise<IdempotencyResponse & { replayed: boolean }> {
  const requestHash = stableJsonSha256(request);
  const existing = await repository.getIdempotency(scope, key);
  if (existing) {
    return { ...replay(existing.requestHash, requestHash, existing.response), replayed: true };
  }

  const reservation = await repository.reserveIdempotency(scope, key, requestHash);
  if (reservation === "exists") {
    const raced = await repository.getIdempotency(scope, key);
    if (!raced) {
      throw new ApiError("idempotency_unavailable", "idempotency reservation could not be read", 503, true);
    }
    return { ...replay(raced.requestHash, requestHash, raced.response), replayed: true };
  }

  let response: IdempotencyResponse;
  try {
    response = await operation();
  } catch (error) {
    await repository.releaseIdempotency(scope, key, requestHash).catch(() => undefined);
    throw error;
  }

  try {
    await repository.finishIdempotency(scope, key, response);
    return { ...response, replayed: false };
  } catch (error) {
    // The business operation may already be committed (or may have completed in R2).
    // Keep the reservation fail-closed so a retry cannot repeat that side effect.
    throw error;
  }
}

function replay(
  storedHash: string,
  requestHash: string,
  response: IdempotencyResponse | undefined,
): IdempotencyResponse {
  if (storedHash !== requestHash) {
    throw conflict("idempotency_key_reused", "the idempotency key was already used for a different request");
  }
  if (!response) {
    throw new ApiError("request_in_progress", "a matching request is still in progress", 409, true);
  }
  return response;
}

export async function runResignableIdempotent<T>(
  repository: IngestionRepository,
  scope: string,
  key: string,
  request: unknown,
  operation: () => Promise<T>,
): Promise<{ value: T; replayed: boolean }> {
  const requestHash = stableJsonSha256(request);
  const existing = await repository.getIdempotency(scope, key);
  if (existing && existing.requestHash !== requestHash) {
    throw conflict("idempotency_key_reused", "the idempotency key was already used for a different request");
  }
  let reserved = false;
  if (!existing) {
    const reservation = await repository.reserveIdempotency(scope, key, requestHash);
    if (reservation === "reserved") {
      reserved = true;
    } else {
      const raced = await repository.getIdempotency(scope, key);
      if (!raced || raced.requestHash !== requestHash) {
        throw conflict("idempotency_key_reused", "the idempotency key was already used for a different request");
      }
    }
  }
  try {
    const response = await operation();
    if (reserved) {
      // Store only the stable fingerprint marker. Presigned bearer URLs are derived again on replay.
      await repository.finishIdempotency(scope, key, {
        statusCode: 204,
        body: { resignable: true },
      });
    }
    return { value: response, replayed: existing !== null || !reserved };
  } catch (error) {
    if (reserved) {
      await repository.releaseIdempotency(scope, key, requestHash).catch(() => undefined);
    }
    throw error;
  }
}
