import { describe, expect, it } from "vitest";

import type { OperatorRole } from "./domain.js";
import { capabilitiesFor, hasCapability, requireCapability, type Capability } from "./rbac.js";

const roles: OperatorRole[] = ["viewer", "operator", "admin"];
const capabilities: Capability[] = [
  "console.read",
  "data.export",
  "clip.preview",
  "identity.write",
  "governance.write",
  "operator.admin",
];

describe("console capabilities", () => {
  it("gives a viewer read access and nothing else", () => {
    expect(capabilitiesFor("viewer")).toEqual(["console.read"]);
  });

  it("reserves operator administration for admins", () => {
    expect(hasCapability("viewer", "operator.admin")).toBe(false);
    expect(hasCapability("operator", "operator.admin")).toBe(false);
    expect(hasCapability("admin", "operator.admin")).toBe(true);
  });

  it("keeps every role strictly contained in the next", () => {
    for (let index = 1; index < roles.length; index += 1) {
      const lower = capabilitiesFor(roles[index - 1]!);
      const higher = capabilitiesFor(roles[index]!);
      expect(higher.length).toBeGreaterThan(lower.length);
      for (const capability of lower) {
        expect(higher).toContain(capability);
      }
    }
  });

  it("throws for every capability a role does not hold", () => {
    for (const role of roles) {
      for (const capability of capabilities) {
        if (hasCapability(role, capability)) {
          expect(() => requireCapability(role, capability)).not.toThrow();
        } else {
          expect(() => requireCapability(role, capability)).toThrow(/insufficient_role|capability/);
        }
      }
    }
  });
});
