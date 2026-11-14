import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import type {
  CatalogManifestCategory,
  CatalogPayloads,
  CatalogValidator,
} from "./signed-catalog.js";

const execFileAsync = promisify(execFile);
const CATEGORIES = [
  "device_profiles",
  "recording_profiles",
  "game_profiles",
  "mappings",
  "depth_profiles",
  "camera_bindings",
] as const satisfies readonly CatalogManifestCategory[];

export function catalogValidatorCargoArgs(
  manifestPath: string,
  platform: NodeJS.Platform = process.platform,
): string[] {
  return [
    ...(platform === "win32" ? ["+stable-x86_64-pc-windows-msvc"] : []),
    "run",
    "--quiet",
    "--manifest-path",
    manifestPath,
    "--features",
    "depth-lab",
    "--",
  ];
}

export class ClientSourceCatalogValidator implements CatalogValidator {
  constructor(
    private readonly validatorBinary = process.env.DATAMINER_CATALOG_VALIDATOR_BIN ?? null,
    private readonly cargo = "cargo",
    private readonly manifestPath = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../../tools/catalog-validator/Cargo.toml",
    ),
  ) {}

  async validate(manifest: Buffer, payloads: CatalogPayloads): Promise<void> {
    const staging = await mkdtemp(path.join(tmpdir(), "dataminer-catalog-validation-"));
    try {
      const payloadDirectory = path.join(staging, "payloads", "sha256");
      await mkdir(payloadDirectory, { recursive: true });
      await writeFile(path.join(staging, "catalog.json"), manifest);
      for (const category of CATEGORIES) {
        for (const bytes of payloads[category]) {
          const digest = createHash("sha256").update(bytes).digest("hex");
          await writeFile(path.join(payloadDirectory, `${digest}.json`), bytes);
        }
      }
      const command = this.validatorBinary ?? this.cargo;
      const args = this.validatorBinary
        ? [staging]
        : [...catalogValidatorCargoArgs(this.manifestPath), staging];
      await execFileAsync(command, args, { windowsHide: true, maxBuffer: 8 * 1024 * 1024 });
    } catch (error) {
      const stderr =
        typeof error === "object" && error !== null && "stderr" in error
          ? String(error.stderr)
          : String(error);
      throw new Error(`client-source catalog validation failed: ${stderr}`, { cause: error });
    } finally {
      await rm(staging, { recursive: true, force: true });
    }
  }
}
