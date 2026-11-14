import { z } from "zod";

// Wire-level bounds only. The service re-validates every source id against the
// configured list and every key/prefix against the source's allowlist — these
// schemas just keep obviously broken input from reaching it.

const sourceIdSchema = z.string().min(1).max(32);
const objectKeySchema = z.string().min(1).max(1024);

export const reviewTreeQuerySchema = z.object({
  source: sourceIdSchema,
  prefix: z.string().max(1024).default(""),
  cursor: z.string().max(4096).optional(),
});

export const reviewObjectQuerySchema = z.object({
  source: sourceIdSchema,
  key: objectKeySchema,
});

export const reviewSignVideoSchema = z.object({
  source: sourceIdSchema,
  key: objectKeySchema,
});

// Viz artifacts (docs/104 §3): pre-generated visualization groups under a
// source's `viz/` prefix. Same wire shape as video signing, separate schema so
// the two whitelists can never blur into each other.
export const reviewSignVizSchema = z.object({
  source: sourceIdSchema,
  key: objectKeySchema,
});

// --- HF reconciliation (docs/33 §6) ---

export const hfReconcileSchema = z.object({
  repoId: z.string().regex(/^[\w.-]+\/[\w.-]+$/, "repo ids look like org/name"),
});

export const hfStatusQuerySchema = z.object({
  source: sourceIdSchema,
  subjectKey: objectKeySchema,
  // The caller's side of the mismatch check: the unit's video size as the tree
  // listing reported it. Optional — without it a match is just "released".
  videoSizeBytes: z.coerce.number().int().positive().optional(),
});

export const hfMarkSchema = z.object({
  source: sourceIdSchema,
  subjectKey: objectKeySchema,
  mark: z.string().trim().min(1).max(64),
  note: z.string().trim().max(280).optional(),
});
