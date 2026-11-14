import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";

import {
  DEV_SIGNING_KEY_ID,
  signDepthCertificationManifest,
} from "./signed-catalog.js";

type DepthCertificationManifest = {
  schema: string;
  approvedRuntimes: unknown[];
  certifications: unknown[];
  deviceProfileGrants?: unknown[];
};

function requireArgument(name: string): string {
  const prefix = `--${name}=`;
  const value = process.argv.find((argument) => argument.startsWith(prefix))?.slice(prefix.length);
  if (!value) {
    throw new Error(`missing ${prefix}<path>`);
  }
  return value;
}

function validateRoot(bytes: Buffer): DepthCertificationManifest {
  const parsed = JSON.parse(bytes.toString("utf8")) as Partial<DepthCertificationManifest>;
  const keys = Object.keys(parsed).sort();
  const expected = ((parsed.schema === "dm-depth-performance-certifications-v2" || parsed.schema === "dm-depth-performance-certifications-v3")
    ? ["approvedRuntimes", "certifications", "deviceProfileGrants", "schema"]
    : ["approvedRuntimes", "certifications", "schema"]).sort();
  if (JSON.stringify(keys) !== JSON.stringify(expected)) {
    throw new Error(`Depth certification manifest has unexpected root fields: ${keys.join(", ")}`);
  }
  if (parsed.schema !== "dm-depth-performance-certifications-v1" && parsed.schema !== "dm-depth-performance-certifications-v2" && parsed.schema !== "dm-depth-performance-certifications-v3") {
    throw new Error(`unsupported Depth certification manifest schema: ${parsed.schema}`);
  }
  if (!Array.isArray(parsed.approvedRuntimes) || !Array.isArray(parsed.certifications)) {
    throw new Error("Depth certification approvedRuntimes and certifications must be arrays");
  }
  if ((parsed.schema === "dm-depth-performance-certifications-v2" || parsed.schema === "dm-depth-performance-certifications-v3") && !Array.isArray(parsed.deviceProfileGrants)) {
    throw new Error("v2/v3 deviceProfileGrants must be an array");
  }
  return parsed as DepthCertificationManifest;
}

export function createSignedDepthCertificationEnvelope(manifestBytes: Buffer): Buffer {
  validateRoot(manifestBytes);
  const signature = signDepthCertificationManifest(manifestBytes);
  return Buffer.from(
    `${JSON.stringify(
      {
        schemaVersion: "dm-signed-depth-certifications-1",
        signingKeyId: DEV_SIGNING_KEY_ID,
        signature: signature.toString("base64"),
        manifest: manifestBytes.toString("base64"),
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
}

async function main(): Promise<void> {
  const input = requireArgument("input");
  const output = requireArgument("output");
  const manifest = await readFile(input);
  const envelope = createSignedDepthCertificationEnvelope(manifest);
  await writeFile(output, envelope, { flag: "wx" });
  process.stdout.write(`Signed Depth certifications: ${input} -> ${output}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error: unknown) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
