import type { OperatorRole } from "./domain.js";
import { forbidden } from "./errors.js";

export type Capability =
  | "console.read"
  | "data.export"
  | "clip.preview"
  // The data-review surface (docs/33 §7.1): bucket tree navigation plus frame
  // and video byte access. Deliberately NOT part of console.read — frames and
  // events are per-event input streams, a step more sensitive than the video
  // preview, so viewers get zero byte access.
  | "review.read"
  | "identity.write"
  | "governance.write"
  | "operator.admin";

const roleCapabilities: Record<OperatorRole, readonly Capability[]> = {
  viewer: ["console.read"],
  operator: [
    "console.read",
    "data.export",
    "clip.preview",
    "review.read",
    "identity.write",
    "governance.write",
  ],
  admin: [
    "console.read",
    "data.export",
    "clip.preview",
    "review.read",
    "identity.write",
    "governance.write",
    "operator.admin",
  ],
};

export function capabilitiesFor(role: OperatorRole): readonly Capability[] {
  return roleCapabilities[role];
}

export function hasCapability(role: OperatorRole, capability: Capability): boolean {
  return roleCapabilities[role].includes(capability);
}

export function requireCapability(role: OperatorRole, capability: Capability): void {
  if (!hasCapability(role, capability)) {
    throw forbidden("insufficient_role", `this action requires the ${capability} capability`);
  }
}
