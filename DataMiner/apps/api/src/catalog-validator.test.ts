import { describe, expect, it } from "vitest";

import { catalogValidatorCargoArgs } from "./catalog-validator.js";

describe("catalog validator cargo command", () => {
  it("pins the MSVC toolchain on Windows", () => {
    expect(catalogValidatorCargoArgs("validator/Cargo.toml", "win32")).toEqual([
      "+stable-x86_64-pc-windows-msvc",
      "run",
      "--quiet",
      "--manifest-path",
      "validator/Cargo.toml",
      "--features",
      "depth-lab",
      "--",
    ]);
  });

  it("uses the host default toolchain outside Windows", () => {
    expect(catalogValidatorCargoArgs("validator/Cargo.toml", "linux")[0]).toBe("run");
  });
});
