import { describe, expect, it } from "vitest";

import {
  generate,
  generateBatch,
  normalize,
  normalizedPattern,
  pretty,
} from "./app-invites.js";

describe("invite code normalization", () => {
  // The whole point of one shared `normalize` is that a code pasted in any
  // plausible form reaches the same row. If these drift apart, issued codes stop
  // matching at redemption.
  it("folds every plausible paste of one code to the same value", () => {
    const canonical = normalize("DM-2345-6789-ABCD");

    expect(canonical).toBe("DM23456789ABCD");
    for (const variant of [
      "dm-2345-6789-abcd",
      "DM23456789ABCD",
      "  DM-2345-6789-ABCD  ",
      "DM 2345 6789 ABCD",
      "DM_2345_6789_ABCD",
    ]) {
      expect(normalize(variant)).toBe(canonical);
    }
  });

  it("survives absent input instead of throwing at the signup boundary", () => {
    expect(normalize(null)).toBe("");
    expect(normalize(undefined)).toBe("");
    expect(normalize("")).toBe("");
  });

  it("normalizes without validating, leaving the shape check to the caller", () => {
    // Normalization is not a filter: garbage folds to garbage, and the pattern
    // (or the table's CHECK) is what rejects it.
    expect(normalize("not a code")).toBe("NOTACODE");
    expect(normalizedPattern.test(normalize("not a code"))).toBe(false);
  });
});

describe("invite code generation", () => {
  it("produces the display form and a normalized form the table accepts", () => {
    for (let attempt = 0; attempt < 200; attempt += 1) {
      const code = generate();
      expect(code).toMatch(/^DM-[0-9A-Z]{4}-[0-9A-Z]{4}-[0-9A-Z]{4}$/);
      expect(normalizedPattern.test(normalize(code))).toBe(true);
    }
  });

  // 0/O/1/I/L are the pairs a person retyping a code confuses. Excluding them is
  // the reason the alphabet is 31 characters rather than 36.
  it("never emits an ambiguous character", () => {
    const emitted = new Set<string>();
    for (let attempt = 0; attempt < 500; attempt += 1) {
      for (const character of normalize(generate()).slice(2)) emitted.add(character);
    }

    for (const ambiguous of ["0", "O", "1", "I", "L"]) {
      expect(emitted.has(ambiguous)).toBe(false);
    }
  });

  // 31^12 ≈ 2^59.5. The guard is against someone shortening the code to make it
  // friendlier to type without noticing what it costs (11 §5.2).
  it("carries at least 59 bits of entropy", () => {
    const body = normalize(generate()).slice(2);
    const bits = body.length * Math.log2(31);

    expect(body).toHaveLength(12);
    expect(bits).toBeGreaterThan(59);
  });

  it("round-trips display and normalized forms", () => {
    const code = generate();

    expect(pretty(normalize(code))).toBe(code);
  });

  it("returns anything that is not a well-formed code untouched", () => {
    // Better to show an odd value verbatim than to render it as a plausible
    // code that does not exist.
    for (const value of ["", "nonsense", "DM123", "DMXXXXXXXXXXXXXX"]) {
      expect(pretty(value)).toBe(value);
    }
  });
});

describe("invite code batches", () => {
  it("returns the requested number of distinct codes", () => {
    const batch = generateBatch(64);

    expect(batch).toHaveLength(64);
    expect(new Set(batch.map(normalize)).size).toBe(64);
  });

  it("returns display forms, because that is what gets handed out", () => {
    for (const code of generateBatch(8)) {
      expect(code).toContain("-");
    }
  });

  it("returns nothing for an empty batch instead of looping", () => {
    expect(generateBatch(0)).toEqual([]);
  });
});
