import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";

import {
  CATALOG_VARY,
  catalogSetEtag,
  resolveCatalogDelivery,
  serializeCatalogSet,
  type CatalogReleaseForDelivery,
} from "./catalog-delivery.js";

const kcd2Id = "dataminer-kingdom-come-deliverance-2-steam";
function release(catalogId: string): CatalogReleaseForDelivery {
  // Noncanonical whitespace is intentional: delivery must retain the stored bytes.
  const envelopeBytes = Buffer.from(`{ "manifest": "${catalogId}", "signature": "unchanged" }\n`);
  return {
    catalogId, catalogVersion: "2026-09-10.0", envelopeBytes,
    envelopeSha256: `sha256:${createHash("sha256").update(envelopeBytes).digest("hex")}`,
  };
}
const previous = Array.from({ length: 30 }, (_, index) => release(`dataminer-existing-${String(index).padStart(2, "0")}`));
const kcd2 = release(kcd2Id);
const published = [kcd2, ...previous].reverse();
const headers = (version: string) => ({
  "x-dataminer-catalog-schema-max": "2",
  "x-dataminer-client-version": version,
});

describe("per-catalog client compatibility", () => {
  it.each(["0.7.21", "0.7.22", "0.7.23"])("keeps the legacy KCD2 release but filters combined-v2 for %s", (version) => {
    expect(resolveCatalogDelivery(published, headers(version)).headers["x-dataminer-catalog-count"]).toBe("31");
    const next = [{ ...kcd2, catalogVersion: "2026-09-11.0" }, ...previous];
    const result = resolveCatalogDelivery(next, headers(version));
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") throw new Error("expected the unchanged prior catalogs");
    expect(result.body).toEqual(serializeCatalogSet(previous));
    expect(result.headers.etag).toBe(catalogSetEtag(serializeCatalogSet(previous)));
    expect(resolveCatalogDelivery(next, { ...headers(version), "if-none-match": result.headers.etag }).kind).toBe("not_modified");
  });

  it.each(["0.7.24", "0.8.0"])("delivers exact combined-v2 bytes to %s", (version) => {
    const next = [{ ...kcd2, catalogVersion: "2026-09-11.0" }, ...previous];
    const result = resolveCatalogDelivery(next, headers(version));
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") throw new Error("expected the new release");
    const ordered = [...next].sort((a, b) => a.catalogId < b.catalogId ? -1 : 1);
    expect(result.body).toEqual(serializeCatalogSet(ordered));
  });

  it.each(["unknown", "2026-09-09.invalid", "2026-02-30.0", "2026-09-11.1", "2027-01-01.0"])("does not assume revision %s uses the old KCD2 source schema", (catalogVersion) => {
    const next = [{ ...kcd2, catalogVersion }, ...previous];
    const result = resolveCatalogDelivery(next, headers("0.7.23"));
    expect(result.headers["x-dataminer-catalog-count"]).toBe("30");
  });

  it.each([
    ["2026-09-17.1", "0.7.32", true],
    ["2026-09-18.0", "0.7.32", false],
    ["2026-09-18.0", "0.7.33", true],
    ["2026-10-01.0", "0.7.32", false],
    ["unknown", "0.7.32", false],
    ["2026-09-18.0", "0.8.0", true],
  ] as const)("CP2077 release %s is consumable by %s: %s", (catalogVersion, version, consumable) => {
    const cp2077 = { ...release("dataminer-cyberpunk-2077-steam-dev"), catalogVersion };
    const result = resolveCatalogDelivery([cp2077, kcd2, ...previous], headers(version));
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") throw new Error("the other catalogs stay consumable");
    const decoded = JSON.parse(result.body.toString("utf8")) as { bundles: Array<{ catalogId: string }> };
    expect(decoded.bundles.some((entry) => entry.catalogId === cp2077.catalogId)).toBe(consumable);
    expect(decoded.bundles.some((entry) => entry.catalogId === kcd2Id)).toBe(true);
  });

  it.each(["0.5.0", "0.7.0", "0.7.20", "0.7.20+build.1"])(
    "keeps all prior 30 envelopes and their exact ETag for %s",
    (version) => {
      const result = resolveCatalogDelivery(published, headers(version));
      expect(result.kind).toBe("ok");
      if (result.kind !== "ok") throw new Error("expected compatible prior set");
      const oldBody = serializeCatalogSet(previous);
      expect(result.body).toEqual(oldBody);
      expect(result.headers.etag).toBe(catalogSetEtag(oldBody));
      expect(result.headers["x-dataminer-catalog-count"]).toBe("30");
      expect(result.headers.vary).toBe(CATALOG_VARY);
      const conditional = resolveCatalogDelivery(published, {
        ...headers(version), "if-none-match": `"other", W/${catalogSetEtag(oldBody)}`,
      });
      expect(conditional.kind).toBe("not_modified");
    },
  );

  it.each(["0.7.21", "0.7.22", "0.8.0", "1.0.0"])("delivers all 31 original envelopes to %s", (version) => {
    const result = resolveCatalogDelivery(published, {
      ...headers(version), "if-none-match": catalogSetEtag(serializeCatalogSet(previous)),
    });
    expect(result.kind).toBe("ok");
    if (result.kind !== "ok") throw new Error("upgrade must fetch the expanded set");
    expect(result.headers["x-dataminer-catalog-count"]).toBe("31");
    const decoded = JSON.parse(result.body.toString("utf8")) as { bundles: Array<{ catalogId: string; envelope: string; envelopeSha256: string }> };
    for (const entry of decoded.bundles) {
      const original = published.find((row) => row.catalogId === entry.catalogId)!;
      expect(Buffer.from(entry.envelope, "base64")).toEqual(original.envelopeBytes);
      expect(entry.envelopeSha256).toBe(original.envelopeSha256);
    }
    expect(resolveCatalogDelivery(published, { ...headers(version), "if-none-match": result.headers.etag }).kind).toBe("not_modified");
  });

  it.each([
    {},
    { "x-dataminer-catalog-schema-max": "2" },
    { "x-dataminer-client-version": "0.7.21" },
    headers("bad-version"),
    headers("0.4.9"),
    { ...headers("0.7.21"), "x-dataminer-catalog-schema-max": "not-a-number" },
    { ...headers("0.7.21"), "x-dataminer-catalog-schema-max": "1" },
    { ...headers("0.7.21"), "x-dataminer-client-version": ["0.7.20", "0.7.21"] },
  ])("rejects invalid compatibility headers before wildcard 304: %j", (invalid) => {
    expect(resolveCatalogDelivery(published, { ...invalid, "if-none-match": "*" }).kind).toBe("incompatible");
  });

  it("does not invent an empty set when nothing compatible is published", () => {
    expect(resolveCatalogDelivery([], {}).kind).toBe("not_published");
    expect(resolveCatalogDelivery([kcd2], headers("0.7.20")).kind).toBe("not_published");
    expect(resolveCatalogDelivery([kcd2], { "if-none-match": "*" }).kind).toBe("incompatible");
    expect(resolveCatalogDelivery([kcd2], headers("0.7.21")).kind).toBe("ok");
  });
});
