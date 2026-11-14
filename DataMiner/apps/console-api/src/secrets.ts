import { createHash, randomBytes, randomInt, timingSafeEqual } from "node:crypto";

import { Algorithm, hash, verify } from "@node-rs/argon2";

// OWASP second-choice profile for argon2id. Kept in one place so a future
// parameter change is a single edit and every stored hash carries its own
// parameters in the PHC string.
const passwordHashOptions = {
  algorithm: Algorithm.Argon2id,
  memoryCost: 65_536,
  timeCost: 3,
  parallelism: 1,
} as const;

export async function hashPassword(password: string): Promise<string> {
  return hash(password, passwordHashOptions);
}

export async function verifyPassword(passwordHash: string, password: string): Promise<boolean> {
  try {
    return await verify(passwordHash, password);
  } catch {
    return false;
  }
}

export function generateOtpCode(): string {
  return String(randomInt(0, 1_000_000)).padStart(6, "0");
}

export function generateOpaqueToken(): string {
  return randomBytes(32).toString("base64url");
}

export function generateSalt(): Buffer {
  return randomBytes(32);
}

export function hashChallengeCode(salt: Buffer, code: string): string {
  return createHash("sha256").update(salt).update(code, "utf8").digest("hex");
}

export function hashSessionToken(token: string): string {
  return createHash("sha256").update(token, "utf8").digest("hex");
}

export function hashSubject(salt: string, subject: string): string {
  return createHash("sha256").update(salt, "utf8").update(subject, "utf8").digest("hex");
}

export function secretEquals(left: string, right: string): boolean {
  const leftBytes = Buffer.from(left, "utf8");
  const rightBytes = Buffer.from(right, "utf8");
  if (leftBytes.length !== rightBytes.length) return false;
  return timingSafeEqual(leftBytes, rightBytes);
}

// Login must cost the same whether or not the address exists, so an unknown
// address is verified against a throwaway hash instead of returning early.
export async function createDecoyPasswordHash(): Promise<string> {
  return hashPassword(randomBytes(32).toString("base64url"));
}
